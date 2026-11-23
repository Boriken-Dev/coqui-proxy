//! Every bound is a setting: its default is the documented value, and changing it changes the
//! behaviour it bounds.

use std::time::Duration;

use coqui_proxy::{ConnectorSettings, PacSettings, ReaderSettings, ResolverSettings};

#[test]
fn the_defaults_are_the_documented_values() {
    let resolver = ResolverSettings::default();
    assert_eq!(resolver.timeout, Duration::from_secs(10));
    assert_eq!(resolver.settings_refresh, Duration::from_secs(60));
    assert_eq!(resolver.source_retry, Duration::from_secs(30));
    assert_eq!(resolver.failure_memory, Duration::from_secs(300));
    assert_eq!(resolver.cache_ttl, Some(Duration::from_secs(60)));
    assert_eq!(resolver.cache_capacity, 1024);
    assert!(!resolver.rotate && !resolver.wpad && !resolver.probe && resolver.system);
    assert_eq!(resolver.rotation_capacity, 1024);
    assert_eq!(resolver.probe_timeout, Duration::from_secs(5));
    assert_eq!(resolver.probe_failure_memory, Duration::from_secs(30));
    assert_eq!(resolver.interface_cache_ttl, Duration::from_secs(10));
    assert_eq!(resolver.bypass_dns_timeout, Duration::from_secs(5));

    let reader = ReaderSettings::default();
    assert_eq!(reader.command_timeout, Duration::from_secs(5));
    assert_eq!(reader.gsettings_cache_ttl, Duration::from_secs(10));
    assert_eq!(reader.winhttp_timeout, Duration::from_secs(10));
    assert_eq!(reader.cfnetwork_deadline, Duration::from_secs(5));
    assert_eq!(reader.dhcp_lease_paths.len(), 3);

    let pac = PacSettings::default();
    assert_eq!(pac.wpad_cache_ttl, Duration::from_secs(300));
    assert_eq!(pac.call_timeout, Duration::from_secs(5));
    assert_eq!(pac.refusal_after_overrun, Duration::from_secs(30));
    assert_eq!((pac.loop_iterations, pac.recursion), (20_000_000, 400));
    assert_eq!(pac.max_script_bytes, 1 << 20);
    assert_eq!(
        (pac.download_timeout, pac.max_redirects),
        (Duration::from_secs(10), 3)
    );
    assert_eq!(pac.download_cache_ttl, Duration::from_secs(300));
    assert_eq!(pac.dns_timeout, Duration::from_secs(2));
    assert_eq!(pac.dns_cache_ttl, Duration::from_secs(30));
    assert_eq!(pac.dns_cache_capacity, 256);
    assert_eq!(pac.wpad_probe_timeout, Duration::from_secs(3));
    assert!(!pac.browser_compatibility);

    let connector = ConnectorSettings::default();
    assert_eq!(connector.timeout, Duration::from_secs(30));
    assert_eq!(connector.user_agent, "coqui-proxy");
    assert_eq!(connector.max_response_head_bytes, 64 * 1024);
    assert_eq!(connector.max_response_headers, 100);
    assert_eq!(connector.happy_eyeballs_delay, Duration::from_millis(250));
}

#[cfg(feature = "pac")]
mod pac {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use coqui_proxy::pac::PacScript;
    use coqui_proxy::system::{SettingsSource, SettingsState, SystemSettings};
    use coqui_proxy::{Environment, Error, PacSettings, Proxy, Resolver, ResolverSettings, Route};

    use super::Duration;

    const COUNTING: &str =
        "var n = 0; function FindProxyForURL(url, host) { n++; return 'PROXY h' + n + ':8080'; }";

    fn counting_resolver(settings: ResolverSettings) -> Resolver {
        let system = SystemSettings::builder(SettingsSource::Windows, SettingsState::Configured)
            .pac_script(COUNTING)
            .build();
        Resolver::builder()
            .environment(Environment::from_pairs([("X", "y")]))
            .settings(system)
            .resolver_settings(settings)
            .build()
    }

    fn first_host(resolver: &Resolver, url: &str) -> String {
        match &resolver.lookup(url).unwrap()[0] {
            Route::Proxy(proxy) => proxy.host().to_string(),
            Route::Direct => "direct".to_string(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_answer_is_reused_for_cache_ttl_and_never_with_none() {
        let resolver = counting_resolver(ResolverSettings::default());
        assert_eq!(first_host(&resolver, "http://a.example/"), "h1");
        assert_eq!(first_host(&resolver, "http://a.example/"), "h1");

        let mut settings = ResolverSettings::default();
        settings.cache_ttl = None;
        let resolver = counting_resolver(settings);
        assert_eq!(first_host(&resolver, "http://a.example/"), "h1");
        assert_eq!(first_host(&resolver, "http://a.example/"), "h2");
    }

    #[test]
    fn the_cache_keeps_cache_capacity_answers_and_drops_the_oldest() {
        let mut settings = ResolverSettings::default();
        settings.cache_capacity = 1;
        let resolver = counting_resolver(settings);
        assert_eq!(first_host(&resolver, "http://a.example/"), "h1");
        assert_eq!(first_host(&resolver, "http://b.example/"), "h2");
        assert_eq!(first_host(&resolver, "http://a.example/"), "h3");
    }

    #[test]
    fn a_failed_proxy_is_last_until_failure_memory_passes() {
        let system = SystemSettings::builder(SettingsSource::Windows, SettingsState::Configured)
            .pac_script(
                "function FindProxyForURL() { return 'PROXY a.example:1; PROXY b.example:2'; }",
            )
            .build();
        let build = |memory: Duration| {
            let mut settings = ResolverSettings::default();
            settings.failure_memory = memory;
            settings.cache_ttl = None;
            Resolver::builder()
                .environment(Environment::from_pairs([("X", "y")]))
                .settings(system.clone())
                .resolver_settings(settings)
                .build()
        };
        let failed = Proxy::parse("http://a.example:1").unwrap();

        let resolver = build(Duration::from_secs(300));
        resolver.report_failure(&failed);
        assert_eq!(first_host(&resolver, "http://x.example/"), "b.example");

        let resolver = build(Duration::from_millis(1));
        resolver.report_failure(&failed);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(first_host(&resolver, "http://x.example/"), "a.example");
    }

    #[test]
    fn a_script_that_loops_is_ended_by_the_loop_limit_setting() {
        let mut settings = PacSettings::default();
        settings.loop_iterations = 1_000;
        let script = PacScript::builder("function FindProxyForURL() { while (true) {} }")
            .settings(&settings)
            .build()
            .unwrap();
        let started = std::time::Instant::now();
        assert!(matches!(
            script.find_proxy_for_url("http://a.example/", "a.example"),
            Err(Error::Pac(_))
        ));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn a_script_recurses_only_as_deep_as_the_recursion_setting() {
        let source = "function d(n) { return n == 0 ? 0 : 1 + d(n - 1); } \
                      function FindProxyForURL() { return d(100) > 0 ? 'DIRECT' : 'DIRECT'; }";
        let mut shallow = PacSettings::default();
        shallow.recursion = 20;
        let script = PacScript::builder(source)
            .settings(&shallow)
            .build()
            .unwrap();
        assert!(script.find_proxy_for_url("http://a/", "a").is_err());
        let script = PacScript::builder(source).build().unwrap();
        assert_eq!(
            script.find_proxy_for_url("http://a/", "a").unwrap(),
            "DIRECT"
        );
    }

    #[test]
    fn a_script_over_max_script_bytes_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("proxy.pac");
        std::fs::write(&path, "function FindProxyForURL() { return 'DIRECT'; }").unwrap();
        let location = path.to_str().unwrap();
        let mut settings = PacSettings::default();
        settings.max_script_bytes = 10;
        assert!(PacScript::from_location(location, &settings).is_err());
        assert!(PacScript::from_location(location, &PacSettings::default()).is_ok());
    }

    #[test]
    fn a_download_follows_max_redirects_and_no_more() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            for stream in listener.incoming().take(8) {
                let mut stream = stream.unwrap();
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                let reply = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        let url = format!("http://127.0.0.1:{port}/proxy.pac");
        let mut settings = PacSettings::default();
        settings.max_redirects = 1;
        let started = std::time::Instant::now();
        assert!(PacScript::from_location(&url, &settings).is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(server);
    }
}

#[cfg(unix)]
#[test]
fn a_helper_program_is_ended_after_command_timeout() {
    // The reader settings reach `SystemSettings::read_with`; with an empty PATH nothing runs and
    // the answer is the unset settings, quickly.
    let mut reader = ReaderSettings::default();
    reader.command_timeout = Duration::from_millis(50);
    let started = std::time::Instant::now();
    let settings = coqui_proxy::system::SystemSettings::read_with(&reader);
    assert!(started.elapsed() < Duration::from_secs(5), "{settings:?}");
}

#[test]
fn what_is_looser_than_the_defaults_is_listed() {
    use coqui_proxy::{
        BypassRules, Dialect, Negotiate, OnMissingEngine, OriginAuth, ProxyConnector, Resolver,
    };

    assert!(Resolver::system().non_defaults().is_empty());
    assert!(ProxyConnector::default().non_defaults().is_empty());
    let resolver = Resolver::builder()
        .on_missing_engine(OnMissingEngine::Direct)
        .no_proxy(BypassRules::parse("<-loopback>", Dialect::NoProxy))
        .build();
    assert_eq!(resolver.non_defaults().len(), 2);
    let connector = ProxyConnector::builder()
        .negotiate(Negotiate::builder().plain_http(true).build().unwrap())
        .build();
    assert_eq!(connector.non_defaults().len(), 1);
    let origin = OriginAuth::builder().plain_http(true).build().unwrap();
    assert_eq!(origin.non_defaults().len(), 1);
    assert!(
        OriginAuth::builder()
            .build()
            .unwrap()
            .non_defaults()
            .is_empty()
    );
}

mod rotation_and_probe {
    use std::net::TcpListener;

    use coqui_proxy::{Environment, Error, Proxy, ProxySetting, Resolver, ResolverSettings, Route};

    fn routes_of(ports: &[u16]) -> ProxySetting {
        ProxySetting::Routes(
            ports
                .iter()
                .map(|port| {
                    Route::Proxy(Proxy::parse(&format!("http://127.0.0.1:{port}")).unwrap())
                })
                .collect(),
        )
    }

    fn ports(routes: &[Route]) -> Vec<u16> {
        routes
            .iter()
            .map(|route| match route {
                Route::Proxy(proxy) => proxy.port(),
                _ => 0,
            })
            .collect()
    }

    fn resolver(setting: ProxySetting, settings: ResolverSettings) -> Resolver {
        Resolver::builder()
            .environment(Environment::from_pairs([("X", "y")]))
            .proxy(setting)
            .resolver_settings(settings)
            .build()
    }

    #[test]
    fn rotation_starts_each_lookup_at_the_next_proxy() {
        let plain = resolver(routes_of(&[1, 2, 3]), ResolverSettings::default());
        assert_eq!(
            ports(&plain.lookup("http://a.example/").unwrap()),
            [1, 2, 3]
        );
        assert_eq!(
            ports(&plain.lookup("http://a.example/").unwrap()),
            [1, 2, 3]
        );

        let mut settings = ResolverSettings::default();
        settings.rotate = true;
        let rotating = resolver(routes_of(&[1, 2, 3]), settings);
        assert_eq!(
            ports(&rotating.lookup("http://a.example/").unwrap()),
            [1, 2, 3]
        );
        assert_eq!(
            ports(&rotating.lookup("http://a.example/").unwrap()),
            [2, 3, 1]
        );
        assert_eq!(
            ports(&rotating.lookup("http://b.example/").unwrap()),
            [3, 1, 2]
        );
    }

    #[test]
    fn rotation_remembers_no_more_answers_than_rotation_capacity() {
        let mut settings = ResolverSettings::default();
        settings.rotate = true;
        settings.rotation_capacity = 1;
        let setting = routes_of(&[1, 2]);
        let rotating = resolver(setting, settings);
        assert_eq!(
            ports(&rotating.lookup("http://a.example/").unwrap()),
            [1, 2]
        );
        assert_eq!(
            ports(&rotating.lookup("http://a.example/").unwrap()),
            [2, 1]
        );
    }

    fn dead_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    #[test]
    fn a_probe_puts_a_reachable_proxy_first_and_fails_when_none_is() {
        let live = TcpListener::bind("127.0.0.1:0").unwrap();
        let live_port = live.local_addr().unwrap().port();
        let dead = dead_port();
        let mut settings = ResolverSettings::default();
        settings.probe = true;
        settings.probe_timeout = std::time::Duration::from_secs(2);

        let probing = resolver(routes_of(&[dead, live_port]), settings.clone());
        assert_eq!(
            ports(&probing.lookup("http://a.example/").unwrap())[0],
            live_port
        );

        let hopeless = resolver(routes_of(&[dead, dead_port()]), settings);
        assert!(matches!(
            hopeless.lookup("http://a.example/"),
            Err(Error::NoReachableProxy)
        ));
    }
}

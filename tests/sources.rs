//! Sources: any `ProxySource` answers for a URL; a chain asks in turn; a resolver takes sources
//! before the environment and the system.

use std::collections::BTreeMap;

use coqui_proxy::system::{SettingsSource, SettingsState, SystemSettings};
use coqui_proxy::{
    BypassRules, ChainSource, Dialect, EnvSource, Environment, Error, OnMissingEngine, Proxy,
    ProxySetting, ProxySource, Resolver, Route, source,
};

fn proxy(text: &str) -> Proxy {
    Proxy::parse(text).unwrap()
}

#[derive(Debug)]
struct NoOpinion;

impl ProxySource for NoOpinion {
    fn routes(&self, _url: &str) -> Result<Option<Vec<Route>>, Error> {
        Ok(None)
    }
}

#[derive(Debug)]
struct Failing;

impl ProxySource for Failing {
    fn routes(&self, _url: &str) -> Result<Option<Vec<Route>>, Error> {
        Err(Error::NoReachableProxy)
    }
}

fn first_host(routes: &[Route]) -> String {
    match &routes[0] {
        Route::Proxy(proxy) => proxy.host().to_string(),
        Route::Direct => "direct".to_string(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_setting_is_read_from_a_proxy_url_a_pac_result_a_location_or_a_script() {
    assert_eq!(
        "direct".parse::<ProxySetting>().unwrap(),
        ProxySetting::Direct
    );
    assert_eq!(
        "http://p.example:3128".parse::<ProxySetting>().unwrap(),
        ProxySetting::Proxy(proxy("http://p.example:3128"))
    );
    assert!(matches!(
        "PROXY a:1; DIRECT".parse::<ProxySetting>().unwrap(),
        ProxySetting::Routes(routes) if routes.len() == 2
    ));
    assert_eq!(
        "http://wpad.corp.example/proxy.pac"
            .parse::<ProxySetting>()
            .unwrap(),
        ProxySetting::Pac("http://wpad.corp.example/proxy.pac".to_string())
    );
    assert!(matches!(
        "prefix+https://gw.example/fetch/".parse::<ProxySetting>().unwrap(),
        ProxySetting::Proxy(gateway) if gateway.is_gateway()
    ));
    assert!("ftp://".parse::<ProxySetting>().is_err());
    assert_eq!(
        ProxySetting::from(proxy("socks5://s.example"))
            .routes()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_chain_asks_in_turn_and_a_failure_stops_it() {
    let chain = ChainSource::new([
        Box::new(NoOpinion) as Box<dyn ProxySource>,
        Box::new(ProxySetting::Proxy(proxy("http://a.example:1"))),
        Box::new(ProxySetting::Direct),
    ]);
    assert_eq!(chain.sources().len(), 3);
    assert_eq!(
        first_host(&source::lookup(&chain, "http://x.example/").unwrap()),
        "a.example"
    );
    assert_eq!(
        source::lookup(&NoOpinion, "http://x.example/").unwrap(),
        [Route::Direct]
    );
    let failing = ChainSource::new([
        Box::new(Failing) as Box<dyn ProxySource>,
        Box::new(ProxySetting::Direct),
    ]);
    assert!(source::lookup(&failing, "http://x.example/").is_err());
}

#[test]
fn a_resolver_asks_its_sources_before_the_environment_and_the_system() {
    let environment = Environment::from_pairs([("HTTP_PROXY", "http://env.example:1")]);
    let resolver = Resolver::builder()
        .environment(environment.clone())
        .source(NoOpinion)
        .source(ProxySetting::Proxy(proxy("http://added.example:2")))
        .build();
    assert_eq!(resolver.sources().len(), 2);
    assert_eq!(
        first_host(&resolver.lookup("http://x.example/").unwrap()),
        "added.example"
    );

    let only_none = Resolver::builder()
        .environment(environment)
        .source(NoOpinion)
        .build();
    assert_eq!(
        first_host(&only_none.lookup("http://x.example/").unwrap()),
        "env.example"
    );
}

#[test]
fn the_environment_is_a_source_with_no_proxy_and_scheme_fallbacks() {
    let environment = Environment::from_pairs([
        ("HTTP_PROXY", "http://h.example:1"),
        ("ALL_PROXY", "socks5://all.example:2"),
        ("NO_PROXY", "skip.example"),
    ]);
    let source = EnvSource::builder().environment(environment).build();
    assert_eq!(source.proxies().len(), 2);
    assert!(source.no_proxy().matches("http://skip.example/"));
    let host = |url: &str| first_host(&source.routes(url).unwrap().unwrap());
    assert_eq!(host("http://a.example/"), "h.example");
    assert_eq!(host("ws://a.example/"), "h.example");
    assert_eq!(host("https://a.example/"), "all.example");
    assert_eq!(host("http://skip.example/"), "direct");

    let explicit = EnvSource::builder()
        .proxies(BTreeMap::from([(
            "https".to_string(),
            ProxySetting::Direct,
        )]))
        .no_proxy(BypassRules::none())
        .build();
    assert_eq!(
        explicit.routes("https://a.example/").unwrap().unwrap(),
        [Route::Direct]
    );
    assert!(explicit.routes("http://a.example/").unwrap().is_none());
}

#[test]
fn scheme_proxies_name_the_first_proxy_by_the_scheme_of_the_url() {
    let resolver = Resolver::builder()
        .environment(Environment::from_pairs([(
            "HTTPS_PROXY",
            "http://p.example:3",
        )]))
        .build();
    let proxies = resolver
        .scheme_proxies("https://a.example/some/path")
        .unwrap();
    assert_eq!(proxies["https"].host(), "p.example");
    assert!(
        resolver
            .scheme_proxies("http://a.example/")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn system_settings_are_a_source_and_bypass_lists_unite() {
    let settings = SystemSettings::builder(SettingsSource::Windows, SettingsState::Configured)
        .proxy("http", proxy("http://sys.example:8"))
        .bypass(BypassRules::parse("corp.example", Dialect::NoProxy))
        .build();
    let source = settings.to_source(
        false,
        std::time::Duration::from_secs(1),
        OnMissingEngine::Error,
    );
    assert_eq!(
        first_host(&source.routes("http://a.example/").unwrap().unwrap()),
        "sys.example"
    );
    assert_eq!(
        source.routes("http://corp.example/").unwrap().unwrap(),
        [Route::Direct]
    );

    let union = BypassRules::parse("a.example", Dialect::NoProxy)
        | BypassRules::parse("b.example", Dialect::NoProxy);
    assert!(union.matches("http://a.example/") && union.matches("http://b.example/"));
    assert!(!union.matches("http://c.example/"));
    assert_eq!(union.entries().count(), 2);
    assert!(!union.is_empty());
}

#[test]
fn a_resolver_reports_its_cache_and_its_bypass() {
    let system = SystemSettings::builder(SettingsSource::Windows, SettingsState::Configured)
        .proxy("http", proxy("http://sys.example:8"))
        .build();
    let resolver = Resolver::builder()
        .environment(Environment::from_pairs([("X", "y")]))
        .settings(system)
        .build();
    assert_eq!(resolver.cache_len(), 0);
    resolver.lookup("http://a.example/").unwrap();
    assert_eq!(resolver.cache_len(), 1);
    resolver.clear();
    assert_eq!(resolver.cache_len(), 0);
    assert!(resolver.no_proxy().is_empty());
}

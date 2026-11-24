# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Which proxy a URL goes through: `Resolver`, the proxy variables, the system's settings (WinHTTP,
  Internet Options, `scutil`, CFNetwork, GNOME, MATE, KDE, NetworkManager, libproxy), PAC and
  WPAD, bypass rules in five dialects with CIDR, `<local>`, `<subnet>` and `<-loopback>`.
- `ProxyConnector`: HTTP `CONNECT` and SOCKS 4/4a/5 with Basic, Negotiate and NTLM login, and
  origin-server login (`OriginAuth`).
- Sans-I/O cores for the `CONNECT` exchange, its login and the SOCKS frames, with decoders that
  take any bytes.
- `ProxySource`, `ChainSource`, `EnvSource`, `SystemSource`, `PacSource`, `WinHttpSource`,
  `CfNetworkSource` and `LibProxySource` (feature `libproxy`); `ResolverBuilder::source`.
- URL-prefix gateways (`prefix+http`, `prefix+https`, `PREFIX` in PAC results), `Proxy::for_url`,
  `Resolver::scheme_proxies`, `BypassRules` union, rotation and probing of answers.
- PAC: custom builtins, files in UTF-8 or Latin-1, downloads, WPAD discovery, browser
  compatibility, reload of stale scripts.
- `asynch` (feature `tokio`): the connector on Tokio over the same sans-I/O cores, Happy Eyeballs
  (`ConnectorSettings::happy_eyeballs_delay`), `asynch::lookup`; `hyper::Connector` (feature
  `hyper`), a `tower` service for hyper's client. `ConnectorSettings::{failover, bad_proxy_ttl}`
  and `ProxyConnectorBuilder::{failover, bad_proxy_ttl}`: a connector remembers its own failed
  proxies and can try only the first route.
- Settings types for every bound (`ResolverSettings`, `ReaderSettings`, `PacSettings`,
  `ConnectorSettings`) and `non_defaults()` on resolvers and connectors.
- `relay` (feature `relay`): `Relay`, `RelayBuilder`, `RelayHandle`, `RelaySettings`: a loopback HTTP proxy
  started inside the process that sends `CONNECT` and absolute-form `http:` requests through the
  connector, with a random per-relay `Proxy-Authorization: Basic` credential, for clients that cannot take
  a connector (`reqwest`) but should still get failover, PAC, bypass rules and proxy login.

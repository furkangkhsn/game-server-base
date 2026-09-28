//! Turning the listener grammar (scalar keys or the `[[listeners]]`
//! array) into the concrete doors the composition root binds.

use std::net::SocketAddr;

use tracing::warn;

use crate::*;

impl ListenerSpec {
    /// The parsed address this listener will claim.
    pub(crate) fn addr(&self) -> SocketAddr {
        match self {
            Self::Tcp { addr }
            | Self::Tls { addr, .. }
            | Self::Udp { addr }
            | Self::Quic { addr, .. }
            | Self::Ws { addr } => *addr,
        }
    }

    /// This listener's transport as the config spells it (a door's name
    /// in a startup error).
    pub(crate) fn transport(&self) -> &'static str {
        match self {
            Self::Tcp { .. } => "tcp",
            Self::Tls { .. } => "tls",
            Self::Udp { .. } => "udp",
            Self::Quic { .. } => "quic",
            Self::Ws { .. } => "ws",
        }
    }
}

/// Reduce the config's listener surface to validated [`ListenerSpec`]s:
///
/// - `listeners` absent → ONE spec derived from the legacy scalar keys,
///   preserving their exact behavior INCLUDING their error variants
///   (half-set `tls_*` → `TlsCertNeedsKey`/`TlsKeyNeedsCert`; `udp` +
///   `tls_*` → `UdpWithTls`), so existing configs keep failing in exactly
///   the ways they always did;
/// - `listeners` non-empty → one spec per entry, each validated on its own
///   ("tls"/"quic" need both files; "tcp"/"udp"/"ws" take none), with a
///   warn when any legacy scalar was also touched (see [`Config::listeners`]);
/// - `listeners` empty → `EmptyListeners`.
///
/// Duplicates are detected over PARSED addresses (not raw strings).
/// Concrete addresses must be unique across entries; port 0 is exempt (see
/// Stage 3 below) because each `:0` entry asks the OS for its OWN free
/// port.
pub(crate) fn resolve_listeners(cfg: &Config) -> Result<Vec<ListenerSpec>, ServerError> {
    // Stage 1 — reduce BOTH config grammars to one internal shape: the
    // entry's transport kind, its (still unparsed) bind string, and its
    // optional TLS files. The tls-file COMBINATION checks are grammar-level
    // policy, so they run here, per entry.
    let mut entries: Vec<(ListenerTransport, String, Option<String>, Option<String>)> = match &cfg
        .listeners
    {
        Some(entries) if entries.is_empty() => return Err(ServerError::EmptyListeners),
        Some(entries) => {
            // Prefer-the-array warning: fire only when a legacy scalar
            // actually differs from its built-in default. The deserializer
            // cannot tell "explicitly set to the default value" from
            // "omitted", so an operator who left every scalar alone gets
            // no noise; one who set both sees which side won.
            let def = Config::default();
            let legacy_touched = cfg.bind != def.bind
                || cfg.transport != def.transport
                || cfg.tls_cert != def.tls_cert
                || cfg.tls_key != def.tls_key;
            if legacy_touched {
                warn!(
                    entries = entries.len(),
                    "`[[listeners]]` takes precedence: ignoring the legacy                          scalar transport keys (transport/bind/tls_cert/tls_key)"
                );
            }
            entries
                .iter()
                .map(|e| {
                    match e.transport {
                        ListenerTransport::Tcp => {
                            if e.tls_cert.is_some() || e.tls_key.is_some() {
                                return Err(ServerError::ListenerTcpWithTls {
                                    bind: e.bind.clone(),
                                });
                            }
                        }
                        ListenerTransport::Tls => {
                            // The variants are STATE-descriptive (the
                            // legacy scalar path set the convention:
                            // cert-set-key-missing → CertNeedsKey), so
                            // each check reports the file it is missing
                            // — a half-set entry must be named by the
                            // message that describes IT. (Found during
                            // the QUIC/WS listener round: this arm had
                            // the two constructors swapped relative to
                            // their texts, untested until now.)
                            if e.tls_cert.is_none() {
                                return Err(ServerError::ListenerTlsKeyNeedsCert {
                                    bind: e.bind.clone(),
                                });
                            }
                            if e.tls_key.is_none() {
                                return Err(ServerError::ListenerTlsCertNeedsKey {
                                    bind: e.bind.clone(),
                                });
                            }
                        }
                        ListenerTransport::Udp => {
                            if e.tls_cert.is_some() || e.tls_key.is_some() {
                                return Err(ServerError::ListenerUdpWithTls {
                                    bind: e.bind.clone(),
                                });
                            }
                        }
                        // QUIC is TLS 1.3 underneath: the exact same
                        // both-files rule as the "tls" door, with its
                        // own error variants so the message names the
                        // right door kind (same state-descriptive
                        // convention — see the Tls arm above).
                        ListenerTransport::Quic => {
                            if e.tls_cert.is_none() {
                                return Err(ServerError::ListenerQuicKeyNeedsCert {
                                    bind: e.bind.clone(),
                                });
                            }
                            if e.tls_key.is_none() {
                                return Err(ServerError::ListenerQuicCertNeedsKey {
                                    bind: e.bind.clone(),
                                });
                            }
                        }
                        ListenerTransport::Ws => {
                            if e.tls_cert.is_some() || e.tls_key.is_some() {
                                return Err(ServerError::ListenerWsWithTls {
                                    bind: e.bind.clone(),
                                });
                            }
                        }
                    }
                    Ok((
                        e.transport,
                        e.bind.clone(),
                        e.tls_cert.clone(),
                        e.tls_key.clone(),
                    ))
                })
                .collect::<Result<Vec<_>, ServerError>>()?
        }
        None => {
            // Legacy derivation: the single-scalar era's exact
            // semantics, INCLUDING its error variants, so existing
            // configs keep failing in exactly the ways they always did.
            match (cfg.tls_cert.is_empty(), cfg.tls_key.is_empty()) {
                (true, true) | (false, false) => {}
                (false, true) => return Err(ServerError::TlsCertNeedsKey),
                (true, false) => return Err(ServerError::TlsKeyNeedsCert),
            }
            if cfg.transport == TransportKind::Udp && !cfg.tls_cert.is_empty() {
                return Err(ServerError::UdpWithTls);
            }
            let (kind, cert, key) = match cfg.transport {
                TransportKind::Udp => (ListenerTransport::Udp, None, None),
                TransportKind::Tcp if !cfg.tls_cert.is_empty() => (
                    ListenerTransport::Tls,
                    Some(cfg.tls_cert.clone()),
                    Some(cfg.tls_key.clone()),
                ),
                TransportKind::Tcp => (ListenerTransport::Tcp, None, None),
            };
            vec![(kind, cfg.bind.clone(), cert, key)]
        }
    };

    // Stage 2 — parse every bind up front: a malformed address is a config
    // error that must fail BEFORE any socket exists (never half-start).
    let mut specs: Vec<ListenerSpec> = Vec::with_capacity(entries.len());
    for (kind, raw_bind, cert, key) in entries.drain(..) {
        let addr: SocketAddr = raw_bind.parse().map_err(|e: std::net::AddrParseError| {
            ServerError::BadBind(raw_bind.clone(), e.to_string())
        })?;
        let spec = match kind {
            ListenerTransport::Tcp => ListenerSpec::Tcp { addr },
            ListenerTransport::Tls => ListenerSpec::Tls {
                addr,
                cert_pem: cert.expect("tls entry validated to carry a cert path"),
                key_pem: key.expect("tls entry validated to carry a key path"),
            },
            ListenerTransport::Udp => ListenerSpec::Udp { addr },
            ListenerTransport::Quic => ListenerSpec::Quic {
                addr,
                cert_pem: cert.expect("quic entry validated to carry a cert path"),
                key_pem: key.expect("quic entry validated to carry a key path"),
            },
            ListenerTransport::Ws => ListenerSpec::Ws { addr },
        };
        specs.push(spec);
    }

    // Stage 3 — duplicate detection over PARSED addresses (not raw
    // strings): two doors claiming ONE concrete address is always a
    // mistake (the second bind could not succeed anyway), and reporting it
    // at config time names the offending entry instead of failing inside a
    // bind syscall. PORT 0 IS EXEMPT, on purpose: a configured `:0` is a
    // request for a different, OS-chosen free port EVERY time — two such
    // entries never end up on the same address, so treating their equal
    // spelling as a collision would make ephemeral-port deployments (and
    // every test suite) impossible while catching nothing real.
    let mut seen: std::collections::HashSet<SocketAddr> =
        std::collections::HashSet::with_capacity(specs.len());
    for spec in &specs {
        let addr = spec.addr();
        if addr.port() != 0 && !seen.insert(addr) {
            return Err(ServerError::DuplicateBind(addr.to_string()));
        }
    }
    Ok(specs)
}

/// Refuse a `listen_backlog` the socket builder would refuse
/// (`gsb_net::listen::listen_backlog_problem`: zero, or past a C `int`)
/// — at startup, before any socket exists, with the key named; the
/// kernel's own cap (`somaxconn`) is not an error.
pub(crate) fn check_listen_backlog(cfg: &Config) -> Result<(), ServerError> {
    match gsb_net::listen::listen_backlog_problem(cfg.listen_backlog) {
        Some(_) => Err(ServerError::BadListenBacklog(cfg.listen_backlog)),
        None => Ok(()),
    }
}

/// Refuse a UDP socket buffer size the socket builder would refuse
/// (`gsb_net::listen::socket_buffer_problem`: below a page, or past a C
/// `int`) — at startup, before any socket exists, with the key named.
/// Unset is not checked (it means "leave the system default"); the
/// kernel's own cap (`rmem_max`/`wmem_max`) is not an error.
pub(crate) fn check_udp_buffers(cfg: &Config) -> Result<(), ServerError> {
    let keys = [
        ("udp_recv_buffer_bytes", cfg.udp_recv_buffer_bytes),
        ("udp_send_buffer_bytes", cfg.udp_send_buffer_bytes),
    ];
    for (key, size) in keys {
        if let Some(value) = size
            && gsb_net::listen::socket_buffer_problem(value).is_some()
        {
            return Err(ServerError::BadUdpBuffer { key, value });
        }
    }
    Ok(())
}

/// The buffer sizes every UDP-based door's socket asks for (B4).
pub(crate) fn udp_buffers(cfg: &Config) -> gsb_net::listen::UdpBuffers {
    gsb_net::listen::UdpBuffers {
        recv: cfg.udp_recv_buffer_bytes,
        send: cfg.udp_send_buffer_bytes,
    }
}

/// Parse the config's 32-hex-char cookie key into 16 bytes (the rUDP
/// cookie key is an operator-supplied alternative to the OS-entropy
/// draw — see `gsb_net::udp::CookieKey`).
pub(crate) fn parse_cookie_key(s: &str) -> Result<[u8; 16], String> {
    if s.len() != 32 {
        return Err(format!("expected 32 hex characters, got {}", s.len()));
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| {
            format!(
                "invalid hex pair `{}:{}` at position {}",
                &s[i * 2..i * 2 + 1],
                &s[i * 2 + 1..i * 2 + 2],
                i * 2
            )
        })?;
    }
    Ok(out)
}

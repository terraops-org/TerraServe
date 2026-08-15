// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Parsing a `postgis://` layer URI, and the type that holds its password.
//!
//! Split out from `postgis.rs` deliberately: this is pure, does no I/O, and carries the one
//! security-critical type in the feature. It must be reviewable and testable without a database.

use std::fmt;

/// A libpq connection string that MUST NOT be printed.
///
/// `Debug` and `Display` are hand-written to redact. Auditing every log line and error path for
/// the password is the approach that eventually fails; a type that cannot print itself is the one
/// that does not. Getting the real value requires calling `expose()`, which greps for easily.
pub struct Dsn(String);

impl Dsn {
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Test-only: wrap an arbitrary string as a `Dsn`.
    ///
    /// Exists so a test can exercise what happens to a MALFORMED connection string. It has to,
    /// because [`parse_postgis_uri`] can no longer produce one: every value it interpolates is
    /// quoted and the port is validated, which is the whole point of `quote_dsn_value` +
    /// `validate_port`. Building the bad input directly is the alternative to triggering a test
    /// on a live defect — a test whose premise is a bug goes vacuous the moment the bug is fixed.
    #[cfg(test)]
    pub(crate) fn from_raw_for_test(s: impl Into<String>) -> Self {
        Dsn(s.into())
    }
}

impl fmt::Debug for Dsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Dsn(<redacted>)")
    }
}

impl fmt::Display for Dsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// A parsed `postgis://` layer target. `Debug` is derived, which is SAFE only because `Dsn`'s own
/// `Debug` redacts — see the test that pins it.
#[derive(Debug)]
pub struct PgTarget {
    pub dsn: Dsn,
    pub schema: String,
    pub table: String,
    pub geom_col: Option<String>,
    pub srid: Option<i32>,
}

/// Parse `postgis://user:${VAR}@host[:port]/db/[schema.]table[?geom=&srid=]`.
///
/// `env` is injected so tests are deterministic and do not mutate process state.
pub fn parse_postgis_uri(
    spec: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<PgTarget, String> {
    // Deliberately does NOT echo `spec` back: this is the first thing that runs, before any
    // userinfo parsing, so an operator who pastes a conventional `postgres://user:pass@host/db`
    // string (an easy mistake — `postgres://` is the standard scheme name) must not have that
    // password land in a `String` error a caller may log. The layer name is already in the
    // caller's context, so naming what failed is enough.
    let rest = spec
        .strip_prefix("postgis://")
        .ok_or_else(|| "layer URI does not start with postgis://".to_string())?;

    let (authority, path) = rest
        .split_once('/')
        .ok_or_else(|| "postgis URI needs /database/table".to_string())?;
    let (userinfo, hostport) = authority
        .rsplit_once('@')
        .ok_or_else(|| "postgis URI needs user@host".to_string())?;

    // Password handling, before anything else touches the string.
    let (user, password) = match userinfo.split_once(':') {
        Some((u, p)) => {
            let var = p
                .strip_prefix("${")
                .and_then(|s| s.strip_suffix('}'))
                .ok_or_else(|| {
                    format!(
                        "layer URI carries a LITERAL password. Use ${{VAR}} and set the \
                         environment variable instead, so the config stays safe to commit."
                    )
                })?;
            let val = env(var).ok_or_else(|| format!("environment variable {var} is not set"))?;
            (u, Some(val))
        }
        None => (userinfo, None),
    };

    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let (db, table_spec) = path
        .split_once('/')
        .ok_or_else(|| "postgis URI needs /database/table".to_string())?;
    let (schema, table) = match table_spec.split_once('.') {
        Some((s, t)) => (s.to_string(), t.to_string()),
        None => ("public".to_string(), table_spec.to_string()),
    };
    if table.is_empty() {
        return Err("postgis URI has an empty table name".to_string());
    }

    let mut geom_col = None;
    let mut srid = None;
    for kv in query.unwrap_or("").split('&').filter(|s| !s.is_empty()) {
        match kv.split_once('=') {
            Some(("geom", v)) => geom_col = Some(v.to_string()),
            Some(("srid", v)) => {
                srid = Some(v.parse::<i32>().map_err(|_| format!("bad srid: {v}"))?)
            }
            // `sslmode` is a real libpq parameter an operator will reasonably reach for, and it is
            // the ONE unknown parameter where the generic "unknown postgis URI parameter" message
            // actively misleads: it reads as a typo, so the operator fixes the spelling, moves on,
            // and keeps a plaintext connection they now believe is encrypted. TerraServe connects
            // with `tokio_postgres::NoTls` today (postgis.rs) and has no TLS override at all, so
            // say that instead. See docs/postgis-layers.md, "Transport security".
            Some(("sslmode", _)) => {
                return Err(
                    "postgis URI parameter `sslmode` is not supported: TerraServe does \
                            NOT yet support TLS to Postgres, and every connection is PLAINTEXT \
                            (password and rows included). Keep the database on a private network \
                            or loopback, or front it with a TLS-terminating proxy / SSH tunnel. \
                            TLS support is planned; see docs/postgis-layers.md."
                        .to_string(),
                )
            }
            Some((k, _)) => return Err(format!("unknown postgis URI parameter: {k}")),
            None => return Err(format!("malformed postgis URI parameter: {kv}")),
        }
    }

    // Every interpolated value is quoted, UNCONDITIONALLY — host, port, user, dbname, password,
    // all five, not just the ones that "look like" they need it. libpq's keyword/value
    // connection-string grammar splits on whitespace, so an unquoted value containing a space does
    // not become part of one setting, it silently becomes TWO. A password of
    // `hunter2 sslmode=disable` concatenated in bare would parse as `password=hunter2` followed by
    // a second `sslmode=disable` keyword, downgrading an encrypted connection to plaintext with no
    // error at all. Quoting always is simpler and safer than guessing which values are "safe".
    //
    // The port was the one value this rule USED to miss (it was interpolated raw), which is how
    // `postgis://ts:${P}@db:5432 sslmode=disable/gis/t` re-opened the injection through the host's
    // port substring after the password route had been closed. It is now both quoted AND validated
    // as digits — quoting alone would turn the injection into a confusing libpq error instead of a
    // clear one, and validation alone would still leave the escaping rule with an exception.
    let (host, port) = split_hostport(hostport)?;
    let mut dsn = format!(
        "host={} user={} dbname={}",
        quote_dsn_value(host),
        quote_dsn_value(user),
        quote_dsn_value(db)
    );
    if let Some(port) = port {
        dsn.push_str(&format!(" port={}", quote_dsn_value(validate_port(port)?)));
    }
    if let Some(p) = password {
        dsn.push_str(&format!(" password={}", quote_dsn_value(&p)));
    }

    Ok(PgTarget {
        dsn: Dsn(dsn),
        schema,
        table,
        geom_col,
        srid,
    })
}

/// Split `host[:port]`, understanding a bracketed IPv6 literal.
///
/// A plain `split_once(':')` is wrong for IPv6: `[::1]:5432` yields host `[` and port `:5432]`,
/// and a bare `::1` yields an empty host with port `:1` — neither errors, both connect somewhere
/// unintended or fail with a libpq message that names neither cause. Brackets are the standard URI
/// form and are understood; an UNBRACKETED literal is rejected with the fix spelled out, because
/// `::1:5432` is genuinely ambiguous (is the last group an address group or a port?).
fn split_hostport(hp: &str) -> Result<(&str, Option<&str>), String> {
    if let Some(rest) = hp.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| format!("postgis URI host `{hp}` opens `[` but never closes `]`"))?;
        if host.is_empty() {
            return Err("postgis URI has an empty bracketed host `[]`".to_string());
        }
        let port = match tail {
            "" => None,
            t => Some(t.strip_prefix(':').ok_or_else(|| {
                format!("postgis URI host `{hp}`: expected `:port` after `]`, found `{t}`")
            })?),
        };
        return Ok((host, port));
    }
    match hp.split_once(':') {
        Some((_, p)) if p.contains(':') => Err(format!(
            "postgis URI host `{hp}` looks like an unbracketed IPv6 address. Write it in \
             brackets: [{hp}] — or [{hp}]:5432 to give a port."
        )),
        Some(("", _)) => Err(format!(
            "postgis URI host `{hp}` has an empty host before `:`"
        )),
        Some((h, p)) => Ok((h, Some(p))),
        None if hp.is_empty() => Err("postgis URI needs user@host".to_string()),
        None => Ok((hp, None)),
    }
}

/// The port must be digits. Echoing it back is safe: `split_hostport`'s input is everything AFTER
/// the last `@`, so it can never contain the userinfo, and therefore never the password.
fn validate_port(p: &str) -> Result<&str, String> {
    match p.parse::<u16>() {
        Ok(n) if n > 0 && p.bytes().all(|b| b.is_ascii_digit()) => Ok(p),
        _ => Err(format!(
            "postgis URI port must be a number 1-65535, found `{p}`"
        )),
    }
}

/// Quote and escape a value for a libpq keyword/value connection string, per
/// <https://www.postgresql.org/docs/current/libpq-connect.html#LIBPQ-CONNSTRING>: wrap it in
/// single quotes, and backslash-escape any single quote or backslash already inside it. Applied
/// to every interpolated value (host, port, user, dbname, password), not only ones that look
/// suspicious — see the injection note at the call site.
fn quote_dsn_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('\'');
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn env_with<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn parses_schema_table_and_interpolates_the_password() {
        let t = parse_postgis_uri(
            "postgis://ts:${PG_PASS}@db:5432/gis/public.parcels?geom=geom&srid=2056",
            &env_with(&[("PG_PASS", "s3cret")]),
        )
        .unwrap();
        assert_eq!(t.schema, "public");
        assert_eq!(t.table, "parcels");
        assert_eq!(t.geom_col.as_deref(), Some("geom"));
        assert_eq!(t.srid, Some(2056));
        assert!(t.dsn.expose().contains("s3cret"));
    }

    #[test]
    fn defaults_schema_to_public_when_omitted() {
        let t = parse_postgis_uri("postgis://ts:${P}@db/gis/parcels", &env_with(&[("P", "x")]))
            .unwrap();
        assert_eq!(t.schema, "public");
        assert_eq!(t.table, "parcels");
    }

    #[test]
    fn a_literal_password_is_rejected() {
        let e =
            parse_postgis_uri("postgis://ts:hunter2@db/gis/parcels", &env_with(&[])).unwrap_err();
        assert!(
            e.contains("${"),
            "the error must tell the operator the fix: {e}"
        );
        assert!(
            !e.contains("hunter2"),
            "the error must NOT echo the password: {e}"
        );
    }

    #[test]
    fn a_missing_env_var_names_the_variable_not_the_value() {
        let e =
            parse_postgis_uri("postgis://ts:${NOPE}@db/gis/parcels", &env_with(&[])).unwrap_err();
        assert!(e.contains("NOPE"));
    }

    #[test]
    fn scheme_mismatch_error_does_not_leak_a_password() {
        // A plausible operator mistake: pasting the standard `postgres://` scheme name instead
        // of `postgis://`. This is checked BEFORE any userinfo parsing, so `Dsn` never even gets
        // constructed — the leak risk here is the raw `spec` landing in the error string itself.
        let e =
            parse_postgis_uri("postgres://ts:hunter2@db/gis/parcels", &env_with(&[])).unwrap_err();
        assert!(
            !e.contains("hunter2"),
            "the scheme-mismatch error must NOT echo the password: {e}"
        );
    }

    #[test]
    fn unknown_query_parameter_is_rejected() {
        let e = parse_postgis_uri(
            "postgis://ts:${P}@db/gis/parcels?foo=bar",
            &env_with(&[("P", "x")]),
        )
        .unwrap_err();
        assert!(
            e.contains("foo"),
            "the error should name the bad parameter: {e}"
        );
    }

    #[test]
    fn malformed_query_fragment_without_equals_is_rejected() {
        let e = parse_postgis_uri(
            "postgis://ts:${P}@db/gis/parcels?geom",
            &env_with(&[("P", "x")]),
        )
        .unwrap_err();
        assert!(
            e.contains("geom"),
            "the error should show the bad fragment: {e}"
        );
    }

    #[test]
    fn missing_closing_brace_in_password_var_is_rejected() {
        // "${NOPE" with no closing brace: strip_prefix("${") succeeds but strip_suffix('}')
        // does not, so this must fail cleanly (not panic, not silently treat "${NOPE" as a
        // literal password to interpolate as-is).
        let e =
            parse_postgis_uri("postgis://ts:${NOPE@db/gis/parcels", &env_with(&[])).unwrap_err();
        assert!(!e.is_empty());
    }

    #[test]
    fn empty_password_var_name_is_rejected() {
        // "${}" strips to an empty variable name, which can never be set — must error, not
        // panic on an empty lookup key.
        let e = parse_postgis_uri("postgis://ts:${}@db/gis/parcels", &env_with(&[])).unwrap_err();
        assert!(!e.is_empty());
    }

    #[test]
    fn at_sign_inside_a_literal_password_does_not_leak_or_confuse_host_parsing() {
        // userinfo/host is split on the LAST '@' precisely so a password containing '@' does not
        // get mistaken for part of the host. Here the password is literal (no `${...}`), so this
        // must still hit the literal-password rejection, with no leak of any part of it.
        let e =
            parse_postgis_uri("postgis://ts:hunter@2@db/gis/parcels", &env_with(&[])).unwrap_err();
        assert!(
            !e.contains("hunter"),
            "must not leak the literal password: {e}"
        );
        assert!(
            !e.contains("hunter@2"),
            "must not leak the literal password: {e}"
        );
    }

    // The following tests parse the built DSN back with `tokio_postgres::Config::from_str` — the
    // SAME libpq-grammar parser `postgis.rs` hands the DSN to in production — rather than
    // asserting on `Ok(_)` or a hand-checked substring. That is deliberate: a test that only
    // checks parsing succeeds would pass against the unquoted, vulnerable version too, since an
    // injected `sslmode=disable` keyword parses just fine, it just silently downgrades security.

    #[test]
    fn password_with_a_space_and_an_sslmode_fragment_does_not_inject_a_setting() {
        let t = parse_postgis_uri(
            "postgis://ts:${P}@db/gis/parcels",
            &env_with(&[("P", "hunter2 sslmode=disable")]),
        )
        .unwrap();
        let cfg = tokio_postgres::Config::from_str(t.dsn.expose())
            .expect("the quoted DSN must still be a valid libpq connection string");
        assert_eq!(
            cfg.get_password(),
            Some("hunter2 sslmode=disable".as_bytes()),
            "the whole password must round-trip as ONE value, not be split on the embedded space"
        );
        assert_eq!(
            cfg.get_ssl_mode(),
            tokio_postgres::config::SslMode::Prefer,
            "an `sslmode=disable` fragment embedded IN the password must not become a real \
             setting that silently downgrades the connection to plaintext"
        );
    }

    #[test]
    fn password_with_a_single_quote_round_trips() {
        let t = parse_postgis_uri(
            "postgis://ts:${P}@db/gis/parcels",
            &env_with(&[("P", "it's a secret")]),
        )
        .unwrap();
        let cfg = tokio_postgres::Config::from_str(t.dsn.expose()).unwrap();
        assert_eq!(cfg.get_password(), Some("it's a secret".as_bytes()));
    }

    #[test]
    fn password_with_a_backslash_round_trips() {
        let t = parse_postgis_uri(
            "postgis://ts:${P}@db/gis/parcels",
            &env_with(&[("P", r"back\slash")]),
        )
        .unwrap();
        let cfg = tokio_postgres::Config::from_str(t.dsn.expose()).unwrap();
        assert_eq!(cfg.get_password(), Some(r"back\slash".as_bytes()));
    }

    #[test]
    fn user_with_whitespace_round_trips() {
        let t = parse_postgis_uri(
            "postgis://ts admin:${P}@db/gis/parcels",
            &env_with(&[("P", "x")]),
        )
        .unwrap();
        let cfg = tokio_postgres::Config::from_str(t.dsn.expose()).unwrap();
        assert_eq!(cfg.get_user(), Some("ts admin"));
    }

    #[test]
    fn dbname_with_whitespace_round_trips() {
        let t = parse_postgis_uri(
            "postgis://ts:${P}@db/gis db/parcels",
            &env_with(&[("P", "x")]),
        )
        .unwrap();
        let cfg = tokio_postgres::Config::from_str(t.dsn.expose()).unwrap();
        assert_eq!(cfg.get_dbname(), Some("gis db"));
    }

    // -- the port, the FIFTH interpolated value ----------------------------------------------
    //
    // The port used to be the one value spliced in raw. That is not a cosmetic gap in the
    // "quote everything" rule: it re-opened the exact connection-string injection the quoting was
    // introduced to close, just through the host's port substring instead of through the password.

    #[test]
    fn a_port_carrying_an_sslmode_fragment_is_rejected_and_cannot_disable_tls() {
        // BEFORE the fix this produced `... port=5432 sslmode=disable password='s3cret'`, which
        // parses as a valid libpq string whose `get_ssl_mode()` is `Disable` — TLS silently off,
        // no error anywhere. Assert BOTH halves: it must be rejected, AND (belt and braces, in
        // case a future change starts accepting a quoted port) no DSN this parser emits may ever
        // come back out of the real libpq parser with SSL turned off.
        let e = parse_postgis_uri(
            "postgis://ts:${P}@db:5432 sslmode=disable/gis/public.parcels",
            &env_with(&[("P", "s3cret")]),
        )
        .unwrap_err();
        assert!(
            e.contains("port"),
            "the error should name the port as the problem: {e}"
        );
        assert!(!e.contains("s3cret"), "the error leaked the password: {e}");

        let ok = parse_postgis_uri(
            "postgis://ts:${P}@db:5432/gis/public.parcels",
            &env_with(&[("P", "s3cret")]),
        )
        .unwrap();
        let cfg = tokio_postgres::Config::from_str(ok.dsn.expose()).unwrap();
        assert_eq!(
            cfg.get_ports(),
            [5432],
            "the good port must survive quoting"
        );
        assert_ne!(
            cfg.get_ssl_mode(),
            tokio_postgres::config::SslMode::Disable,
            "nothing in a parsed URI may switch the connection to plaintext through the DSN"
        );
    }

    #[test]
    fn a_non_numeric_port_is_rejected_by_name() {
        let e = parse_postgis_uri(
            "postgis://ts:${P}@db:notaport/gis/t",
            &env_with(&[("P", "x")]),
        )
        .unwrap_err();
        assert!(e.contains("notaport"), "name the bad value: {e}");
        // 0 and out-of-range are equally unusable and must not reach libpq either.
        for bad in ["0", "65536", "-1", "+80", "80 "] {
            assert!(
                parse_postgis_uri(
                    &format!("postgis://ts:${{P}}@db:{bad}/gis/t"),
                    &env_with(&[("P", "x")])
                )
                .is_err(),
                "port `{bad}` must be rejected"
            );
        }
    }

    #[test]
    fn a_bracketed_ipv6_host_keeps_its_address_and_its_port() {
        // `split_once(':')` gives host `[` and port `:1]:5432` here — a host that resolves to
        // nothing and a port that is not a number, reported (if at all) as a libpq parse error
        // naming neither.
        let t = parse_postgis_uri(
            "postgis://ts:${P}@[::1]:5432/gis/t",
            &env_with(&[("P", "x")]),
        )
        .unwrap();
        let cfg = tokio_postgres::Config::from_str(t.dsn.expose()).unwrap();
        assert_eq!(cfg.get_ports(), [5432]);
        assert_host_is(&cfg, "::1");

        // ...and without a port.
        let t =
            parse_postgis_uri("postgis://ts:${P}@[::1]/gis/t", &env_with(&[("P", "x")])).unwrap();
        let cfg = tokio_postgres::Config::from_str(t.dsn.expose()).unwrap();
        assert_host_is(&cfg, "::1");
        assert!(cfg.get_ports().is_empty() || cfg.get_ports() == [5432]);
    }

    fn assert_host_is(cfg: &tokio_postgres::Config, want: &str) {
        match cfg.get_hosts() {
            [tokio_postgres::config::Host::Tcp(h)] => assert_eq!(h, want),
            other => panic!("expected exactly one TCP host `{want}`, got {other:?}"),
        }
    }

    #[test]
    fn an_unbracketed_ipv6_host_is_rejected_with_the_fix() {
        // Genuinely ambiguous (`::1:5432` — address group, or port?), so this refuses rather than
        // guessing. The message must show the bracketed form.
        let e = parse_postgis_uri("postgis://ts:${P}@::1:5432/gis/t", &env_with(&[("P", "x")]))
            .unwrap_err();
        assert!(e.contains("[::1:5432]"), "show the bracketed form: {e}");
    }

    // -- sslmode ------------------------------------------------------------------------------

    #[test]
    fn sslmode_is_rejected_as_unsupported_tls_not_as_a_typo() {
        // TerraServe connects with `tokio_postgres::NoTls` and has no TLS path yet. `sslmode` is a
        // real libpq parameter, so the generic "unknown postgis URI parameter: sslmode" it used to
        // get is the worst possible answer: it reads as a spelling mistake, and an operator who
        // "fixes" it walks away believing the connection is encrypted when every byte, password
        // included, is in the clear. The message must say TLS is unsupported, not that the
        // parameter is unrecognised.
        for v in ["require", "verify-full", "disable", "prefer"] {
            let e = parse_postgis_uri(
                &format!("postgis://ts:${{P}}@db/gis/t?sslmode={v}"),
                &env_with(&[("P", "x")]),
            )
            .unwrap_err();
            assert!(
                e.to_lowercase().contains("plaintext") && e.contains("TLS"),
                "sslmode={v} must state that connections are plaintext / TLS unsupported: {e}"
            );
            assert!(
                !e.contains("unknown postgis URI parameter"),
                "sslmode={v} must not read as a typo: {e}"
            );
        }
        // An actual typo still gets the generic message, naming the parameter.
        let e = parse_postgis_uri(
            "postgis://ts:${P}@db/gis/t?sslmodo=require",
            &env_with(&[("P", "x")]),
        )
        .unwrap_err();
        assert!(e.contains("sslmodo"), "name the bad parameter: {e}");
    }

    #[test]
    fn dsn_never_prints_the_password() {
        let t = parse_postgis_uri(
            "postgis://ts:${P}@db/gis/parcels",
            &env_with(&[("P", "s3cret")]),
        )
        .unwrap();
        assert!(
            !format!("{:?}", t.dsn).contains("s3cret"),
            "Debug leaked the password"
        );
        assert!(
            !format!("{}", t.dsn).contains("s3cret"),
            "Display leaked the password"
        );
        // The whole struct must be safe to log too — a derived Debug on PgTarget would leak.
        assert!(
            !format!("{:?}", t).contains("s3cret"),
            "PgTarget Debug leaked the password"
        );
    }
}

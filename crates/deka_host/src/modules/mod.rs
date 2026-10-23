// Host capability ops: fs, db, net, crypto, concurrency.

use bumpalo::Bump;
use simien_core::op2;
use mysql::prelude::Queryable;
use mysql::{OptsBuilder, Params as MyParams, Pool as MyPool, Value as MyValue};

use ::security::security_policy::{RuleList, SecurityPolicy, parse_deka_security_policy};
use prost::Message as ProstMessage;
use rusqlite::types::ValueRef as SqliteValueRef;
use rusqlite::{Connection as SqliteConnection, params_from_iter as sqlite_params_from_iter};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{File as StdFile, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Protobuf schemas for the host fs/db/net bridges.
mod proto {
    pub mod bridge_v1 {
        include!(concat!(env!("OUT_DIR"), "/deka.bridge.v1.rs"));
    }
}

pub mod http;

mod bridge_metrics;
mod compat;
mod concurrency;
mod crypto;
mod db;
mod db_pg;
mod fs;
mod fs_bridge;
mod net;
#[cfg(test)]
mod net_tests;
mod security;
#[cfg(test)]
mod security_context_tests;
mod security_hint;

pub use security::{
    enforce_capability, enforce_net_public, enforce_net_public_with, security_policy_from_context, simien_db_allowed,
};

fn core_err(msg: impl Into<String>) -> simien_core::error::CoreError {
    simien_core::error::CoreError::from(std::io::Error::other(msg.into()))
}

simien_core::extension!(
    php_core,
    ops = [
        fs::op_php_read_file_sync,
        fs::op_php_write_file_sync,
        fs::op_php_mkdirs,
        security::op_php_env_capability_granted,
        crypto::op_php_sha256,
        crypto::op_php_random_bytes,
        crypto::op_php_digest,
        crypto::op_php_hmac,
        crypto::op_php_secure_compare,
        crypto::op_php_aes_256_gcm_encrypt,
        crypto::op_php_aes_256_gcm_decrypt,
        crypto::op_php_bcrypt_verify,
        crypto::op_php_read_env,
        db::op_php_db_call_proto,
        db::op_php_db_proto_encode,
        db::op_php_db_proto_decode,
        net::op_php_net_call_proto,
        net::op_php_net_proto_encode,
        net::op_php_net_proto_decode,
        fs_bridge::op_php_fs_call_proto,
        fs_bridge::op_php_fs_call_proto_async,
        fs_bridge::op_php_fs_proto_encode,
        fs_bridge::op_php_fs_proto_decode,
        bridge_metrics::op_php_bridge_proto_stats,
        fs::op_php_cwd,
        fs::op_php_canonicalize,
        fs::op_php_file_exists,
        fs::op_php_path_resolve,
        fs::op_php_read_dir,
        compat::op_deka_http_call,
        concurrency::op_php_concurrency_lock_acquire,
        concurrency::op_php_concurrency_lock_release,
    ],
    state = |state| state.put(net::NetState::new()),
);

pub fn init() -> simien_core::Extension {
    php_core::init()
}

/// Same as [`init`], but the net bridge enforces the given policy instead
/// of resolving the per-execution security context on every dispatch.
/// Tests use this to give each isolate its own policy rather than
/// depending on the executing thread's context (deka#537); production
/// installs the context per request and calls [`init`].
pub fn init_with_net_policy(policy: SecurityPolicy) -> simien_core::Extension {
    let mut extension = init();
    let base_state_fn = extension.op_state_fn.take();
    extension.op_state_fn = Some(Box::new(move |state| {
        if let Some(base) = base_state_fn {
            base(state);
        }
        state.put(net::NetState::with_policy(policy.clone()));
    }));
    extension
}

#[cfg(test)]
mod tests {
    use super::crypto::bcrypt_verify_impl;
    use super::db::{
        db_action_payload_to_proto_request, db_call_impl, db_call_proto_impl,
        db_proto_response_to_json,
    };
    use super::fs_bridge::{
        fs_action_payload_to_proto_request, fs_call_impl, fs_call_proto_impl,
        fs_proto_response_to_json,
    };
    use super::net::{
        NetState, net_action_payload_to_proto_request, net_call_impl, net_call_proto_impl,
        net_proto_response_to_json,
    };
    use super::security_hint::default_allow_target_for_capability;
    use super::*;
    use prost::Message;
    use std::net::TcpListener;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn php_extension_has_no_legacy_esm_runtime_module() {
        let extension = init();

        assert!(extension.esm_files.is_empty());
    }

    fn unique_suffix() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{}_{}", std::process::id(), nanos)
    }

    fn assert_ok(value: &serde_json::Value) {
        assert_eq!(
            value.get("ok").and_then(|v| v.as_bool()),
            Some(true),
            "expected ok response, got: {}",
            value
        );
    }

    #[test]
    fn default_allow_target_uses_wildcard_for_scope_less_capabilities() {
        assert_eq!(default_allow_target_for_capability("env"), Some("*"));
        assert_eq!(default_allow_target_for_capability("net"), Some("*"));
        assert_eq!(default_allow_target_for_capability("run"), Some("*"));
        assert_eq!(default_allow_target_for_capability("db"), Some("*"));
        assert_eq!(default_allow_target_for_capability("wasm"), Some("*"));
        assert_eq!(default_allow_target_for_capability("read"), None);
        assert_eq!(default_allow_target_for_capability("write"), None);
    }

    #[test]
    #[ignore = "requires security capabilities (db grant) not available in unit tests"]
    fn db_proto_open_parity_postgres_mysql_sqlite() {
        let suffix = unique_suffix();
        let cases = vec![
            (
                "postgres",
                serde_json::json!({
                    "host": "127.0.0.1",
                    "port": 5432,
                    "database": format!("db_proto_pg_{}", suffix),
                    "user": "u",
                    "password": "p",
                }),
            ),
            (
                "mysql",
                serde_json::json!({
                    "host": "127.0.0.1",
                    "port": 3306,
                    "database": format!("db_proto_my_{}", suffix),
                    "user": "u",
                    "password": "p",
                }),
            ),
            (
                "sqlite",
                serde_json::json!({
                    "path": format!("/tmp/db_proto_open_{}.sqlite", suffix),
                }),
            ),
        ];

        for (driver, config) in cases {
            let payload = serde_json::json!({
                "driver": driver,
                "config": config
            });

            let json_res =
                db_call_impl("open".to_string(), payload.clone()).expect("json open failed");
            assert_ok(&json_res);
            let json_handle = json_res
                .get("handle")
                .and_then(|v| v.as_u64())
                .expect("json open missing handle");

            let proto_req = db_action_payload_to_proto_request("open", &payload)
                .expect("proto request build failed");
            let proto_bytes = proto_req.encode_to_vec();
            let proto_resp_bytes =
                db_call_proto_impl(&proto_bytes).expect("proto open dispatch failed");
            let proto_resp = proto::bridge_v1::DbResponse::decode(proto_resp_bytes.as_slice())
                .expect("decode failed");
            let proto_json = db_proto_response_to_json(&proto_resp);
            assert_ok(&proto_json);
            let proto_handle = proto_json
                .get("handle")
                .and_then(|v| v.as_u64())
                .expect("proto open missing handle");

            let close_json = db_call_impl(
                "close".to_string(),
                serde_json::json!({ "handle": json_handle }),
            )
            .expect("json close failed");
            assert_ok(&close_json);
            if proto_handle != json_handle {
                let close_proto = db_call_impl(
                    "close".to_string(),
                    serde_json::json!({ "handle": proto_handle }),
                )
                .expect("proto handle close failed");
                assert_ok(&close_proto);
            }
        }
    }

    #[test]
    #[ignore = "requires security capabilities (db grant) not available in unit tests"]
    fn db_proto_sqlite_exec_query_parity() {
        let suffix = unique_suffix();
        let path = format!("/tmp/db_proto_query_{}.sqlite", suffix);
        let open_payload = serde_json::json!({
            "driver": "sqlite",
            "config": { "path": path }
        });
        let open_res = db_call_impl("open".to_string(), open_payload).expect("open failed");
        assert_ok(&open_res);
        let handle = open_res
            .get("handle")
            .and_then(|v| v.as_u64())
            .expect("missing handle");

        let setup_sql = vec![
            "create table if not exists packages (name text, downloads integer)",
            "delete from packages",
            "insert into packages(name, downloads) values ('db', 10), ('component', 20)",
        ];
        for sql in setup_sql {
            let exec_res = db_call_impl(
                "exec".to_string(),
                serde_json::json!({
                    "handle": handle,
                    "sql": sql,
                    "params": []
                }),
            )
            .expect("exec failed");
            assert_ok(&exec_res);
        }

        let json_query = db_call_impl(
            "query".to_string(),
            serde_json::json!({
                "handle": handle,
                "sql": "select name, downloads from packages order by downloads asc",
                "params": []
            }),
        )
        .expect("json query failed");
        assert_ok(&json_query);

        let json_query_again = db_call_impl(
            "query".to_string(),
            serde_json::json!({
                "handle": handle,
                "sql": "select name, downloads from packages order by downloads asc",
                "params": []
            }),
        )
        .expect("json query again failed");
        assert_ok(&json_query_again);

        let proto_req = db_action_payload_to_proto_request(
            "query",
            &serde_json::json!({
                "handle": handle,
                "sql": "select name, downloads from packages order by downloads asc",
                "params": []
            }),
        )
        .expect("proto query request build failed");
        let proto_resp =
            db_call_proto_impl(&proto_req.encode_to_vec()).expect("proto query failed");
        let proto_decoded =
            proto::bridge_v1::DbResponse::decode(proto_resp.as_slice()).expect("decode failed");
        let proto_json = db_proto_response_to_json(&proto_decoded);
        assert_ok(&proto_json);

        assert_eq!(json_query.get("rows"), proto_json.get("rows"));

        let json_stats =
            db_call_impl("stats".to_string(), serde_json::json!({})).expect("json stats failed");
        assert_ok(&json_stats);
        assert!(
            json_stats
                .get("statement_cache_entries")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                >= 1
        );
        assert!(
            json_stats
                .get("statement_cache_hits")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                >= 1
        );
        assert!(
            json_stats
                .get("statement_cache_misses")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                >= 1
        );

        let proto_stats_req = db_action_payload_to_proto_request("stats", &serde_json::json!({}))
            .expect("proto stats request build failed");
        let proto_stats_resp =
            db_call_proto_impl(&proto_stats_req.encode_to_vec()).expect("proto stats failed");
        let proto_stats_decoded = proto::bridge_v1::DbResponse::decode(proto_stats_resp.as_slice())
            .expect("decode stats failed");
        let proto_stats_json = db_proto_response_to_json(&proto_stats_decoded);
        assert_ok(&proto_stats_json);
        assert_eq!(
            json_stats.get("statement_cache_entries"),
            proto_stats_json.get("statement_cache_entries")
        );
        assert_eq!(
            json_stats.get("statement_cache_hits"),
            proto_stats_json.get("statement_cache_hits")
        );
        assert_eq!(
            json_stats.get("statement_cache_misses"),
            proto_stats_json.get("statement_cache_misses")
        );

        let close_res = db_call_impl("close".to_string(), serde_json::json!({ "handle": handle }))
            .expect("close failed");
        assert_ok(&close_res);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    #[ignore = "requires security capabilities (write grant) not available in unit tests"]
    fn fs_proto_binary_roundtrip_integrity() {
        let suffix = unique_suffix();
        let path = format!("/tmp/fs_proto_roundtrip_{}.bin", suffix);
        let payload = serde_json::json!({
            "path": path,
            "data": [0, 1, 2, 10, 127, 128, 200, 255]
        });

        let json_write = fs_call_impl("write_file".to_string(), payload.clone())
            .expect("json write_file failed");
        assert_ok(&json_write);

        let proto_write_req = fs_action_payload_to_proto_request("write_file", &payload)
            .expect("fs proto write request build failed");
        let proto_write_resp = fs_call_proto_impl(&proto_write_req.encode_to_vec())
            .expect("fs proto write_file dispatch failed");
        let proto_write_decoded = proto::bridge_v1::FsResponse::decode(proto_write_resp.as_slice())
            .expect("fs decode write response failed");
        let proto_write_json = fs_proto_response_to_json(&proto_write_decoded);
        assert_ok(&proto_write_json);

        let json_read = fs_call_impl("read_file".to_string(), serde_json::json!({ "path": path }))
            .expect("json read_file failed");
        assert_ok(&json_read);

        let proto_read_req =
            fs_action_payload_to_proto_request("read_file", &serde_json::json!({ "path": path }))
                .expect("fs proto read request build failed");
        let proto_read_resp = fs_call_proto_impl(&proto_read_req.encode_to_vec())
            .expect("fs proto read_file dispatch failed");
        let proto_read_decoded = proto::bridge_v1::FsResponse::decode(proto_read_resp.as_slice())
            .expect("fs decode read response failed");
        let proto_read_json = fs_proto_response_to_json(&proto_read_decoded);
        assert_ok(&proto_read_json);

        assert_eq!(json_read.get("data"), proto_read_json.get("data"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    #[ignore = "requires security capabilities (net grant) not available in unit tests"]
    fn net_proto_tcp_parity_sanity() {
        let mut net_state = NetState::new();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0_u8; 64];
                let n = std::io::Read::read(&mut stream, &mut buf).expect("read");
                std::io::Write::write_all(&mut stream, &buf[..n]).expect("write");
            }
        });

        let json_connect = net_call_impl(
            &mut net_state,
            "connect".to_string(),
            serde_json::json!({
                "host": "127.0.0.1",
                "port": addr.port(),
                "timeout_ms": 3000
            }),
        )
        .expect("json connect failed");
        assert_ok(&json_connect);
        let json_handle = json_connect
            .get("handle")
            .and_then(|v| v.as_u64())
            .expect("json handle missing");

        let json_write = net_call_impl(
            &mut net_state,
            "write".to_string(),
            serde_json::json!({
                "handle": json_handle,
                "data": "ping"
            }),
        )
        .expect("json write failed");
        assert_ok(&json_write);

        let json_read = net_call_impl(
            &mut net_state,
            "read".to_string(),
            serde_json::json!({
                "handle": json_handle,
                "max_bytes": 4
            }),
        )
        .expect("json read failed");
        assert_ok(&json_read);
        assert_eq!(
            json_read.get("data"),
            Some(&serde_json::json!([112, 105, 110, 103]))
        );

        let json_close = net_call_impl(
            &mut net_state,
            "close".to_string(),
            serde_json::json!({ "handle": json_handle }),
        )
        .expect("json close failed");
        assert_ok(&json_close);

        let proto_connect_req = net_action_payload_to_proto_request(
            "connect",
            &serde_json::json!({
                "host": "127.0.0.1",
                "port": addr.port(),
                "timeout_ms": 3000
            }),
        )
        .expect("proto connect build failed");
        let proto_connect_resp =
            net_call_proto_impl(&mut net_state, &proto_connect_req.encode_to_vec())
                .expect("proto connect failed");
        let proto_connect_json = net_proto_response_to_json(
            &proto::bridge_v1::NetResponse::decode(proto_connect_resp.as_slice())
                .expect("decode connect"),
        );
        assert_ok(&proto_connect_json);
        let proto_handle = proto_connect_json
            .get("handle")
            .and_then(|v| v.as_u64())
            .expect("proto handle missing");

        let proto_write_req = net_action_payload_to_proto_request(
            "write",
            &serde_json::json!({
                "handle": proto_handle,
                "data": [112, 111, 110, 103]
            }),
        )
        .expect("proto write build failed");
        let proto_write_resp =
            net_call_proto_impl(&mut net_state, &proto_write_req.encode_to_vec())
                .expect("proto write failed");
        let proto_write_json = net_proto_response_to_json(
            &proto::bridge_v1::NetResponse::decode(proto_write_resp.as_slice())
                .expect("decode write"),
        );
        assert_ok(&proto_write_json);

        let proto_read_req = net_action_payload_to_proto_request(
            "read",
            &serde_json::json!({
                "handle": proto_handle,
                "max_bytes": 4
            }),
        )
        .expect("proto read build failed");
        let proto_read_resp = net_call_proto_impl(&mut net_state, &proto_read_req.encode_to_vec())
            .expect("proto read failed");
        let proto_read_json = net_proto_response_to_json(
            &proto::bridge_v1::NetResponse::decode(proto_read_resp.as_slice())
                .expect("decode read"),
        );
        assert_ok(&proto_read_json);
        assert_eq!(
            proto_read_json.get("data"),
            Some(&serde_json::json!([112, 111, 110, 103]))
        );

        let proto_close_req = net_action_payload_to_proto_request(
            "close",
            &serde_json::json!({ "handle": proto_handle }),
        )
        .expect("proto close build failed");
        let proto_close_resp =
            net_call_proto_impl(&mut net_state, &proto_close_req.encode_to_vec())
                .expect("proto close failed");
        let proto_close_json = net_proto_response_to_json(
            &proto::bridge_v1::NetResponse::decode(proto_close_resp.as_slice())
                .expect("decode close"),
        );
        assert_ok(&proto_close_json);

        server.join().expect("server join");
    }

    #[test]
    fn proto_bridge_rejects_malformed_payloads() {
        assert!(db_call_proto_impl(&[0xff, 0x00, 0x01]).is_err());
        assert!(fs_call_proto_impl(&[0xff, 0x00, 0x01]).is_err());
        assert!(net_call_proto_impl(&mut NetState::new(), &[0xff, 0x00, 0x01]).is_err());
    }

    #[test]
    fn bcrypt_verify_known_hash() {
        // Hash of "password123" generated with bcrypt cost 10.
        let hash = "$2b$10$DqpfeHg1RhyMilY/GTQvgeahRja6yf5aL8dYoH6EwABQY.CZ.pnNu";
        let result = bcrypt_verify_impl("password123".to_string(), hash.to_string());
        assert_ok(&result);
        assert_eq!(result.get("valid").and_then(|v| v.as_bool()), Some(true));

        let bad = bcrypt_verify_impl("wrongpassword".to_string(), hash.to_string());
        assert_ok(&bad);
        assert_eq!(bad.get("valid").and_then(|v| v.as_bool()), Some(false));
    }

    #[test]
    fn bcrypt_verify_rejects_passwords_over_72_bytes() {
        let password = "a".repeat(72);
        let overlong = format!("{}extra", password);
        let hash = bcrypt::hash(&password, 4).expect("hash 72-byte bcrypt password");

        let valid = bcrypt_verify_impl(password, hash.clone());
        assert_ok(&valid);
        assert_eq!(valid.get("valid").and_then(|v| v.as_bool()), Some(true));

        let rejected = bcrypt_verify_impl(overlong, hash);
        assert_ok(&rejected);
        assert_eq!(rejected.get("valid").and_then(|v| v.as_bool()), Some(false));
    }
}

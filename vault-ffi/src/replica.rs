// SPDX-License-Identifier: MIT
//! Replica targets as container backends, and replication outcomes.

use std::sync::Arc;

use vault_container::backend::{
    ContainerBackend, LocalBackend, S3Backend, S3Config, WebDavBackend, WebDavConfig,
};
use vault_container::replicate::{ReplicaOutcome, ReplicaStatus};
use zeroize::Zeroizing;

use crate::error::FfiError;
use crate::types::{ReplicaOutcomeFfi, ReplicaResult, ReplicaTarget};

/// A backend for `target`. Remote targets trust the bundled webpki roots plus
/// `extra_ca_pem` if set; plain http is accepted only for loopback hosts.
/// Credentials move into zeroizing buffers of the backend config.
pub(crate) async fn backend_for(
    target: ReplicaTarget,
    extra_ca_pem: Option<&str>,
) -> Result<Arc<dyn ContainerBackend>, FfiError> {
    let ca = extra_ca_pem.map(str::to_owned);
    Ok(match target {
        ReplicaTarget::Local { path } => {
            if path.is_empty() {
                return Err(FfiError::invalid("empty replica path"));
            }
            Arc::new(LocalBackend::open(path).await?)
        }
        ReplicaTarget::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
            secret_access_key,
            path_style,
        } => Arc::new(S3Backend::new(S3Config {
            endpoint,
            region,
            bucket,
            prefix,
            access_key: access_key_id,
            secret_key: Zeroizing::new(secret_access_key),
            path_style,
            allow_insecure_http: false,
            extra_root_ca_pem: ca,
        })?),
        ReplicaTarget::WebDav {
            url,
            user,
            password,
        } => Arc::new(WebDavBackend::new(WebDavConfig {
            base_url: url,
            username: user,
            password: Zeroizing::new(password),
            allow_insecure_http: false,
            extra_root_ca_pem: ca,
        })?),
    })
}

pub(crate) fn result_of(s: ReplicaStatus) -> ReplicaResult {
    let outcome = match s.outcome {
        ReplicaOutcome::Synced {
            seq,
            uploaded,
            deleted,
        } => ReplicaOutcomeFfi::Synced {
            seq,
            uploaded: uploaded as u64,
            deleted: deleted as u64,
        },
        ReplicaOutcome::Rollback {
            found_seq,
            last_seen_seq,
            seq,
        } => ReplicaOutcomeFfi::Rollback {
            found_seq,
            last_seen_seq,
            seq,
        },
        ReplicaOutcome::Diverged {
            replica_seq,
            primary_seq,
            ..
        } => ReplicaOutcomeFfi::Diverged {
            replica_seq,
            primary_seq,
        },
        ReplicaOutcome::Adopted { seq, rollback, .. } => {
            ReplicaOutcomeFfi::Adopted { seq, rollback }
        }
        ReplicaOutcome::Failed { error } => ReplicaOutcomeFfi::Failed { error },
    };
    ReplicaResult {
        id: s.id.0,
        outcome,
    }
}

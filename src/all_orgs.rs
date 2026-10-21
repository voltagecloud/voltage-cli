//! `wallets list --all-orgs`: the wallets in every organization a browser login can see, in
//! one list.
//!
//! The API has no listing across organizations, and each organization's wallets need a token
//! for that organization. So the login lists its organizations, then each organization's token
//! exchange and wallet read run side by side, a few at a time. Each wallet gains its
//! organization's name, in organization order. An API key belongs to one organization, so
//! it cannot list others.
//!
//! When some organizations fail, the wallets of the others are still written, and the command
//! then fails with each failed organization in `error.detail`, so a script never mistakes a
//! partial list for a complete one. When every organization fails, nothing is written. When
//! they all fail the same way, as every exchange does for a revoked login, that error is
//! reported as is, with its own hint and detail.

use crate::{
    Error, Result, api,
    auth::{Discovery, Resolver},
    cli::{ApiInvocation, GlobalFlags},
    config::{Credential, Scope},
    error::{ErrorDetail, ErrorKind},
    output::{Envelope, Outcome, Output},
    terminal::Terminal,
};
use futures_util::{StreamExt, stream};
use serde::Serialize;
use serde_json::{Value, json};
use std::borrow::Cow;
use uuid::Uuid;

/// Organizations read at once: enough to hide the latency of a few round trips, without
/// sending the auth service a burst for a login in many organizations.
const CONCURRENCY: usize = 4;

/// An organization whose wallets could not be listed, and why.
#[derive(Clone, Debug, Serialize)]
pub struct FailedOrganization {
    pub organization_id: Uuid,
    pub name: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The failure's own detail, such as `credential_problem` for a revoked login.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<ErrorDetail>,
    /// Gives the exit code when this is the first organization that failed.
    #[serde(skip)]
    pub kind: ErrorKind,
}

impl FailedOrganization {
    /// The organization's failure as the error it was.
    fn into_error(self) -> Error {
        let error = Error::new(self.kind, self.message);
        let error = match self.hint {
            Some(hint) => error.with_hint(hint),
            None => error,
        };
        match self.detail {
            Some(detail) => error.with_detail(detail),
            None => error,
        }
    }
}

struct Organization {
    id: Uuid,
    name: String,
}

pub async fn wallets(
    invocation: &ApiInvocation,
    global: &GlobalFlags,
    resolver: Resolver<'_>,
    scope: &Scope,
    out: &mut Output,
) -> Result<()> {
    let Credential::Login(login) = resolver.resolve(scope).await? else {
        return Err(Error::auth(
            "--all-orgs needs a browser login, but the selected credential is an API key",
        )
        .with_hint(
            "An API key belongs to one organization: list its wallets with --org ID instead, or select a browser login with --account NAME.",
        ));
    };
    let listed = resolver
        .list(scope, Discovery::Organizations, &login.access_token)
        .await?;
    let organizations = organizations(&listed);
    let token = &login.access_token;
    // The exchanges run side by side, so only the one line below shows progress.
    let quiet = Terminal::new(true, global.no_input);
    let reads = stream::iter(&organizations)
        .map(|organization| async move {
            let scope = Scope {
                org: Some(organization.id),
                account: scope.account.clone(),
                ..Scope::default()
            };
            let token = resolver.organization_token(&scope, token, quiet).await?;
            api::organization_wallets(invocation, global, token, organization.id).await
        })
        .buffered(CONCURRENCY)
        .collect::<Vec<_>>();
    let label = format!(
        "Listing wallets in {} organizations...",
        organizations.len()
    );
    let results = global.terminal().during(label, reads).await;
    let (listed, failed) = merge(&organizations, results);
    if !failed.is_empty() && failed.len() == organizations.len() {
        return Err(every_failed(failed));
    }
    out.write(
        Envelope::new(None, Value::Array(listed), None, Outcome::Retrieved),
        &[],
    )?;
    if failed.is_empty() {
        Ok(())
    } else {
        Err(partial(failed, organizations.len()))
    }
}

/// The organizations the login lists that carry a UUID `id`, in the order listed.
fn organizations(listed: &Value) -> Vec<Organization> {
    listed
        .as_array()
        .or_else(|| listed.get("items").and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.parse().ok()?;
            let name = item.get("name").and_then(Value::as_str).unwrap_or("-");
            Some(Organization {
                id,
                name: name.to_owned(),
            })
        })
        .collect()
}

/// Each listed wallet with its organization's ID and name, and the organizations that failed.
fn merge(
    organizations: &[Organization],
    results: Vec<Result<Value>>,
) -> (Vec<Value>, Vec<FailedOrganization>) {
    let mut wallets = Vec::new();
    let mut failed = Vec::new();
    for (organization, result) in organizations.iter().zip(results) {
        let listed = match result {
            Ok(listed) => listed,
            Err(error) => {
                failed.push(FailedOrganization {
                    organization_id: organization.id,
                    name: organization.name.clone(),
                    message: error.message,
                    hint: error.hint.map(Cow::into_owned),
                    detail: error.detail,
                    kind: error.kind,
                });
                continue;
            }
        };
        let items = match listed {
            Value::Array(items) => items,
            Value::Object(mut page) => match page.remove("items") {
                Some(Value::Array(items)) => items,
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        wallets.extend(items.into_iter().map(|mut wallet| {
            if let Value::Object(fields) = &mut wallet {
                fields.insert("organization_id".into(), json!(organization.id));
                fields.insert("organization_name".into(), json!(organization.name));
            }
            wallet
        }));
    }
    (wallets, failed)
}

/// The error when no organization could be listed: their shared failure when they all failed
/// the same way, or else each one's.
fn every_failed(mut failed: Vec<FailedOrganization>) -> Error {
    let first = &failed[0];
    let same = failed
        .iter()
        .all(|other| other.kind == first.kind && other.message == first.message);
    if same {
        return failed.swap_remove(0).into_error();
    }
    let total = failed.len();
    partial(failed, total)
}

/// The error for organizations that could not be listed, once the others are written. Its
/// exit code is the first failure's, so a rejected login still exits 3.
fn partial(failed: Vec<FailedOrganization>, total: usize) -> Error {
    let kind = failed.first().map_or(ErrorKind::Api, |failed| failed.kind);
    let names: Vec<String> = failed
        .iter()
        .map(|failed| format!("{} ({})", failed.name, failed.organization_id))
        .collect();
    let hint = if failed.len() == total {
        "Check the login with `voltage auth status`, or list one organization with `voltage wallets list --org ID`.".to_owned()
    } else {
        "The other organizations' wallets are listed. Retry, or list one organization with `voltage wallets list --org ID`.".to_owned()
    };
    Error::new(
        kind,
        format!(
            "Could not list wallets in {} of {total} organizations: {}",
            failed.len(),
            names.join(", ")
        ),
    )
    .with_hint(hint)
    .with_detail(ErrorDetail::FailedOrganizations {
        failed_organizations: failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";

    fn organization(id: &str, name: &str) -> Organization {
        Organization {
            id: id.parse().unwrap(),
            name: name.into(),
        }
    }

    #[test]
    fn organizations_without_a_uuid_are_skipped_and_pages_are_read() {
        let listed = json!([{"id": A, "name": "Acme"}, {"id": "x"}, {"id": B}]);
        let read: Vec<(Uuid, String)> = organizations(&listed)
            .into_iter()
            .map(|organization| (organization.id, organization.name))
            .collect();
        assert_eq!(
            read,
            vec![
                (A.parse().unwrap(), "Acme".into()),
                (B.parse().unwrap(), "-".into())
            ]
        );
        assert_eq!(organizations(&json!({"items": [{"id": A}]})).len(), 1);
        assert!(organizations(&json!(null)).is_empty());
    }

    #[test]
    fn merged_wallets_carry_their_organization_and_failures_are_kept() {
        let organizations = [
            organization(A, "Acme"),
            organization(B, "Beta"),
            organization(A, "Again"),
        ];
        let results = vec![
            Ok(json!([{"id": "w1"}, "not an object"])),
            Err(Error::auth("denied").with_hint("check")),
            Ok(json!({"items": [{"id": "w2"}]})),
        ];
        let (wallets, failed) = merge(&organizations, results);
        assert_eq!(
            wallets,
            vec![
                json!({"id": "w1", "organization_id": A, "organization_name": "Acme"}),
                json!("not an object"),
                json!({"id": "w2", "organization_id": A, "organization_name": "Again"}),
            ]
        );
        assert_eq!(failed.len(), 1);
        assert_eq!(
            serde_json::to_value(&failed[0]).unwrap(),
            json!({"organization_id": B, "name": "Beta", "message": "denied", "hint": "check"})
        );
        assert_eq!(failed[0].kind, ErrorKind::Auth);
        // A page without items, or any other body, adds nothing.
        let (wallets, _) = merge(
            &[organization(A, "a"), organization(B, "b")],
            vec![Ok(json!({"next": null})), Ok(json!(7))],
        );
        assert!(wallets.is_empty());
    }

    #[test]
    fn the_error_names_each_failed_organization() {
        let failed = |hint: Option<&str>| FailedOrganization {
            organization_id: A.parse().unwrap(),
            name: "Acme".into(),
            message: "denied".into(),
            hint: hint.map(str::to_owned),
            detail: None,
            kind: ErrorKind::Auth,
        };
        let some = partial(vec![failed(None)], 2);
        assert_eq!(
            some.message,
            format!("Could not list wallets in 1 of 2 organizations: Acme ({A})")
        );
        assert_eq!(some.kind, ErrorKind::Auth);
        assert!(some.hint.unwrap().starts_with("The other organizations'"));
        let all = partial(vec![failed(Some("h"))], 1);
        assert!(all.hint.unwrap().contains("voltage auth status"));
        assert_eq!(
            serde_json::to_value(all.detail.unwrap()).unwrap()["failed_organizations"][0]["hint"],
            "h"
        );
    }

    #[test]
    fn organizations_that_all_fail_alike_report_that_one_failure() {
        let failure = |id: &str, kind, message: &str| FailedOrganization {
            organization_id: id.parse().unwrap(),
            name: "o".into(),
            message: message.into(),
            hint: Some("sign in again".into()),
            detail: Some(ErrorDetail::Payment(json!({"why": message}))),
            kind,
        };
        let alike = every_failed(vec![
            failure(A, ErrorKind::Auth, "expired"),
            failure(B, ErrorKind::Auth, "expired"),
        ]);
        assert_eq!(
            (alike.kind, alike.message.as_str()),
            (ErrorKind::Auth, "expired")
        );
        assert_eq!(alike.hint.as_deref(), Some("sign in again"));
        assert_eq!(
            serde_json::to_value(alike.detail.unwrap()).unwrap(),
            json!({"why": "expired"})
        );
        // A different kind or message lists each organization instead.
        for (kind, message) in [(ErrorKind::Api, "expired"), (ErrorKind::Auth, "other")] {
            let mixed = every_failed(vec![
                failure(A, ErrorKind::Auth, "expired"),
                failure(B, kind, message),
            ]);
            assert!(
                mixed
                    .message
                    .starts_with("Could not list wallets in 2 of 2"),
                "{}",
                mixed.message
            );
            let detail = serde_json::to_value(mixed.detail.unwrap()).unwrap();
            assert_eq!(detail["failed_organizations"][1]["detail"]["why"], message);
        }
        let without = FailedOrganization {
            hint: None,
            detail: None,
            ..failure(A, ErrorKind::Api, "plain")
        }
        .into_error();
        assert!(without.hint.is_none() && without.detail.is_none());
    }
}

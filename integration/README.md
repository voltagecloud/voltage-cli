# Coordinated service changes

These patches contain the complete auth-service and frontend implementation, including new files. They are kept here so the CLI repository does not depend on ignored development checkouts.

| Repository | Patch | Base commit |
| --- | --- | --- |
| [auth-service](***REMOVED-PRIVATE-REPO***) | [auth-service.patch](auth-service.patch) | `4060df4a9b8da84e51c29ef23984b5f536a76e8e` |
| [frontend-turbo](***REMOVED-PRIVATE-REPO***) | [frontend-turbo.patch](frontend-turbo.patch) | `beea4b8319d151dcaf888ab3c986a183234e3cc0` |

Patch SHA-256 values and base-application verification are recorded in [manifest.json](manifest.json). Each patch was applied to files from its base commit, then compared byte-for-byte with the implemented files.

In a clean checkout of the matching repository and base commit:

```sh
git switch -c codex/cli-device-login
git apply --check /path/to/voltage-cli/integration/auth-service.patch
git apply /path/to/voltage-cli/integration/auth-service.patch
```

Use `frontend-turbo.patch` in the frontend repository. Do not apply over unrelated uncommitted changes. If applying to a newer revision, resolve changes there and rerun the repository checks.

The isolated development copies remain at `.work/auth-service` and `.work/frontend-turbo`, each on `codex/cli-device-login` with its implementation staged. No service changes have been pushed, merged, or deployed. Review [verification](../docs/verification.md) and the [staging rollout](../docs/integration.md) before release.

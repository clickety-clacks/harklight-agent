# Harklight Agent repository guidance

## CI, small changes, and releases

- Keep changes small and independently reviewable. Merge passing PRs to `main` promptly while the Agent remains usable. If `main` turns red, prioritize fixing or reverting the change that caused it; do not hold an unrelated fix behind unfinished work without a real dependency.
- Preserve the existing `CI` workflow. Pull requests and pushes to `main` run `cargo build --locked`, `cargo test --locked`, and the fleet-delivery Python unit tests on Ubuntu and macOS.
- When unfinished behavior can safely ship disabled, use a real feature flag that defaults off and gates the behavior itself, including any background work or server endpoint the feature adds. Test the production-like configuration with the flag off and the intended configuration with it on. Do not add a flag framework unless a feature needs one. A flag cannot make compilation, startup, an incompatible migration, or a shared dependency safe to release.
- Release a passing build independently of feature readiness when the feature is disabled. Enable it separately after its required checks and acceptance pass, then remove temporary rollout flags when the rollout is complete.
- A `vX.Y.Z` tag starts the release workflow; the tag must match the version in `Cargo.toml`. The workflow tests and builds the four macOS and Linux targets, creates archives and checksums, generates and install-checks the Homebrew formula, publishes a GitHub release, and opens a Homebrew tap pull request when its token is available. `workflow_dispatch` is a dry run that creates artifacts without publishing a release or opening that pull request.
- Publishing a GitHub release starts the fleet-delivery workflow, which reconciles the managed fleet to the newest published stable release. Treat release publication as a deployment action.

# Automatic release delivery

The fleet workflow runs when a release is published and on a ten-minute retry
schedule. It selects the newest published stable release that is not withdrawn,
verifies the platform archive against that release's SHA256SUMS, and reconciles
each configured destination independently. Prereleases and drafts are ignored.
The scheduled run also applies changes to the private withdrawn-version list and
retries hosts that were offline or failed earlier.

The deployment runner must use the generic harklight-agent-fleet label, have
the existing SSH access already configured, and receive HARKLIGHT_FLEET_CONFIG
in its process environment. The path it names must point to a private JSON file
outside both the checkout and the Actions workspace. That file contains the
destination aliases and platform targets, withdrawn versions, and a persistent
private readback path. Do not put those values in this repository, workflow
variables, logs, or Actions artifacts.

The runner needs Python 3.10 or later and the existing `ssh` and `scp` commands.
Each destination needs a POSIX shell, `tar`, and either `sha256sum` or `shasum`.

The private file has this shape:

    {
      "hosts": [
        {
          "destination": "configured-ssh-alias",
          "target": "x86_64-unknown-linux-gnu"
        }
      ],
      "withdrawn_versions": [],
      "readback_path": "/var/lib/harklight-agent-fleet/readback.json"
    }

The destination is an existing SSH alias from the runner's SSH configuration;
the target is one of the four release archive platform names. The readback file
is atomically replaced after each reconciliation and is the private latest
readback for all configured destinations. It includes target and withdrawn
versions, each destination's platform, active version output, resolved binary
path, binary SHA-256, prior release retained, attempt time, and outcome.
When a destination is unreachable, its current values remain unknown and the
last successful version/path/hash observation is kept separately with its
observation time.

The workflow prints only aggregate counts and the selected release version.
Per-destination command output and errors are captured into the private
readback. An offline or failing destination does not fail the job or stop the
other attempts. The next scheduled run retries it. The aggregate marks delivery
complete only when every configured destination reports the selected release.
When all published stable releases are withdrawn, no destination is changed and
the private readback records that there is no target.

Each destination attempt downloads its platform archive and verifies its
checksum against the published SHA256SUMS independently, so one corrupted
download does not hold back another destination on the same platform. The
controller copies those exact verified bytes over the existing SSH route. The
remote installer checks the copied archive before extracting that same file. It
tests the candidate binary's exact version before activating it, keeps existing
release directories intact, and replaces the active symlink with an atomic
rename. The workflow does not change CLI behavior, package contents, or release
metadata.

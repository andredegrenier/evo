# Appendix: `evo serve` on AWS

**Nothing in this file has been run.** It is an outline kept for the day the
Debian box is not the answer — a second location, a machine that stays up, or
an ARM chip that is simply faster than the one you own. The primary deployment
is [RUNBOOK.md](RUNBOOK.md), on hardware you already have and already pay for.

---

## The gate

This appendix is **not** something an agent executes. Two things are required
before a single `aws` command runs, and both are the operator's:

1. **Credentials.** No agent provisions anything with an AWS identity it was
   handed in passing.
2. **A typed cost acknowledgment.** The operator types the monthly figure they
   are accepting, in their own words, in their own message. Not a "yes", not an
   approved plan — the number. Instances bill by the hour whether or not
   anyone is reading a PDF on them, and an EC2 instance nobody remembered is
   the most common way this goes wrong.

Absent either, the answer is the Debian box.

---

## Costs

`us-east-1`, on-demand, as verified at the time of writing (2026-08). Prices
move; re-check before committing to any of them.

| Shape | What it is for | Compute | Storage + IP | Monthly |
|---|---|---|---|---|
| `t4g.small` + external model | evo serves; chat goes to a model elsewhere | ~$12 | ~$6 | **~$18** |
| `c7g.xlarge` (4 vCPU) | the built-in 4B, slowly | ~$106 | ~$6 | **~$112** |
| `c7g.2xlarge` (8 vCPU) | the built-in 4B, comfortably | $0.290/hr ≈ $212 | ~$6 | **~$218** |

Storage and address, for all three:

- 30 GB gp3 EBS: **~$2.40/mo**
- one Elastic IP attached to a running instance: **~$3.65/mo** (IPv4 is charged
  per address-hour now, attached or not)
- S3 for blobs: $0.023/GB-mo plus requests — pennies for a personal library
- data out: 100 GB/mo free, then $0.09/GB

For scale: the whole Debian box costs the electricity it was already using.
`c7g.2xlarge` is roughly $2,600 a year to read PDFs on a phone. If chat is the
only reason to want a big instance, `t4g.small` plus an external model endpoint
is a tenth of the price and probably faster.

---

## Outline

Plain `aws` CLI, in order. Substitute your own names throughout.

### 1. Network and access

- Security group `evo-sg`:
  - inbound **443** from `0.0.0.0/0` — the service
  - inbound **80** from `0.0.0.0/0` — only if using Caddy's HTTP-01 challenge
  - inbound **22** from **your operator IP /32 only**, never from anywhere
  - outbound: default (all)
- Better still, skip 22 entirely and use SSM Session Manager, which needs no
  inbound rule at all.
- Even better for the phone: put Tailscale on the instance and open **nothing**.
  Everything RUNBOOK.md §4(a) says applies identically here, and it is the only
  option on this list with no public attack surface.

### 2. Bucket and role

- Private bucket, block-all-public-access on, default SSE (SSE-S3 is free;
  SSE-KMS adds per-request cost), versioning on if you want undelete.
- An IAM role for the instance, scoped to *that bucket and that prefix*:
  `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject`, `s3:HeadObject` on
  `arn:aws:s3:::<bucket>/docs/*` (or `<bucket>/<prefix>/docs/*` if you set one),
  plus `s3:ListBucket` on the bucket.
- Attach it as an instance profile. `object_store` picks the role up through
  the ordinary provider chain (IMDSv2) — **no keys in the config file**, which
  is why `serve/config.json` has nowhere to put any.

### 3. Instance

- Ubuntu 24.04 LTS, **arm64** (Graviton), from the current AMI in your region.
- 30 GB gp3 root volume.
- Instance profile from step 2; security group from step 1.
- IMDSv2 required (`--metadata-options HttpTokens=required`).
- Allocate and associate an Elastic IP.

### 4. Build

Same two paths as Debian, with the architectures reversed:

- **Native on the instance** — same apt list as RUNBOOK.md Path A, plus rustup.
  Graviton builds llama.cpp quickly; this is the easy path.
- **Docker from the Mac** — and here Apple Silicon is finally the *right* host:
  `./deploy/debian/build-in-docker.sh --platform linux/arm64` builds
  **natively**, no emulation, the exact inverse of the x86 situation. Note the
  base image must be no newer than the instance's distribution.

```sh
cargo build --release --features s3
```

### 5. Install and configure

`install.sh` and `evo.service` from this directory work unchanged — Ubuntu is
Debian enough, the package names are the same, and the unit hard-codes nothing
AWS-specific. Then in `/var/lib/evo/serve/config.json`:

```json
{
  "blobs": { "s3": { "bucket": "your-evo-bucket", "prefix": "evo" } }
}
```

`prefix` goes *in front of* evo's own `docs/`, so that writes
`evo/docs/<sha256>.pdf`. Leave it out and the objects sit at `docs/<sha256>.pdf`
at the root of the bucket.

The redb database, the tantivy index and the page cache stay on EBS whatever
`blobs` says: they are memory-mapped files, and object storage is not a
filesystem.

### 6. TLS and a name

- Tailscale (RUNBOOK.md §4a) — nothing to own, nothing open. Recommended.
- Otherwise Caddy with a domain you control, exactly as in RUNBOOK.md §4c.
- `sslip.io` (`evo.<ip-with-dashes>.sslip.io`) works without owning a domain,
  **with a caveat**: Let's Encrypt rate limits are per registered domain, and
  `sslip.io` is one registered domain shared by everyone using it. It is
  frequently at its limit. Fine for a demo, not for something you depend on.

---

## Migrating the Debian box to AWS

Because the formats are shared, this is a copy rather than a conversion:

1. Build the same evo, `--features s3`, on the instance.
2. `sudo systemctl stop evo` on both ends.
3. `rsync -a /var/lib/evo/ instance:/var/lib/evo/` — database, index, sidecars,
   credentials and all. The TOTP enrolment comes with it; the phone does not
   need to re-enrol.
4. Optionally push the PDFs into the bucket and set `blobs` to the S3 form.
   Keys are `docs/<sha256>.pdf`, content-addressed, so uploading the same file
   twice is a no-op.
5. Start it, point the tunnel at the new host.

Reversing it is the same command with the arguments swapped, which is the real
argument for keeping the Debian box as the primary.

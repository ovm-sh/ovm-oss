# Component versions and release identity

OVM has one installed bundle version. Its three Rust binaries (`ovm`,
`ovm-claudex`, `ovm-codex-skew`) inherit `[workspace.package].version` from
the root `Cargo.toml`. They install, update, and roll back together. Release CI
checks every manifest-declared package against the exact tag. npm and Brew
distribution metadata derive the same version from Cargo metadata.

Bundled plugins resolve beside the running OVM binary, so an earlier unrelated
installation on PATH cannot silently replace part of a selected snapshot.
`OVM_ALLOW_PLUGIN_OVERRIDE=1` explicitly enables PATH-first resolution for local
development. A missing bundled plugin is otherwise an incomplete installation.
Third-party `ovm-*` plugins continue to resolve on PATH.

## What changes when

| Change | Identity that changes |
| --- | --- |
| CLI or bundled plugin behavior | OVM bundle version |
| Benchmark or gate implementation | Tool source revision/digest |
| New upstream release, model, or verification evidence | Registry snapshot revision |
| Breaking data format | Schema version, with a client compatibility plan |
| Site layout or copy | Site Git/deployment revision |
| New recording or edit | Media manifest and output hashes |

An internal tool's package version is not its complete execution identity.
Benchmark builds embed a digest of their source and build inputs. Original
measurement records preserve that identity, platform, sanitized measurement
settings, prompt digest, input revisions, and requested/observed model. Existing
records without provenance remain unknown. Reports that combine measurements
from different runs must not label all metrics with one run's identity.

Registry `schema_version` describes format compatibility; `snapshot_revision`
identifies semantic content. Timestamp refreshes do not alter it, but release
dates, model first-seen dates, version membership and verdicts do. Product
revisions are bound into the aggregate index. The registry producer and gate
stamp after their final mutations. The digest encoding is defined by the
snapshot helper; benchmark input hashes use a separate JSON encoding namespace.

## Release manifests

Each new tagged build produces `ovm-<tag>.release.json`. Its schema version is
independent of the OVM version. It binds:

- the release tag and source commit;
- the build role (`qualification` or `public`);
- the installer and binary-bundle manifest hashes;
- all four platform archive hashes, sizes, and individual binary hashes;
- optional matching media manifests and their final output hashes.

Public artifacts are rebuilt from the exported public source, and their
manifest is attested by the public release workflow. Private qualification
artifacts have their own manifest; they are not claimed to be the public bytes.
The draft and finalize gates re-create the public manifest from the downloaded
artifacts and tagged source before accepting it. An identity manifest records
what was built, not an approval or a substitute for runtime acceptance.

The binary archive manifest `ovm-bundle-v1.tsv` remains the small format that
installers understand. The release JSON is a separate release asset, so adding
provenance does not change the installer archive layout or old clients.
Historical tags without the new generator retain their original asset contract.

## Release-bound media and site selection

Recording and site deployment commands run in the maintainer checkout.
One recording run resolves one exact public tag and records all required takes.
An explicitly selected draft can be recorded before publication:

```sh
./scripts/media/record-release-tapes.sh v0.1.8 --draft
```

The draft must already be built in the public repository. Preparation reads its
assets using authenticated GitHub GET requests; recording receives no GitHub
token environment. Published releases use anonymous preparation by default.
Finalizing the same tag and bytes preserves the recording identity. This allows
the release and its site cut to be prepared together before either is published.

The mirror serves the original OVM archive and tagged installer. Its local product fixtures are
identified in the take manifest. A loopback recording does not prove public
network installation. Before cutting, the builder rejects changed or mixed
candidate inputs. Each cut produces an adjacent JSON manifest.

Keep the published release manifest immutable. After approving a cut, generate
a separate site release selection using the same downloaded public archives and
a checkout of the exact public source commit:

```sh
python3 scripts/release-manifest.py \
  --tag "$TAG" --source-commit "$PUBLIC_COMMIT" \
  --source-root "$PUBLIC_CHECKOUT" --artifacts-dir "$ARTIFACTS" \
  --media-manifest site/media/ovm-hatch-v1.mp4.json --media-root . \
  --output site/release.json
```

This verifies the candidate's archive, installer and source identities against
the media, checks the final video/poster bytes, and records the association.
Use the same command with `--verify site/release.json` instead of `--output`
to revalidate the complete selection. Public media provenance uses repository
relative paths; the recording cache's transport metadata must not be published.

Commit the site selection and its matching video, poster and cut sidecar
together. The deployment script verifies this selection and its media bytes
before preparing output. Once release cut sidecars exist, a missing selection
or an unrepresented cut blocks deployment. Historical media without sidecars
continues to use the existing deployment contract.

A stable version bump builds a new candidate; an alpha recording must
not be relabeled as stable evidence. Human readability/seam approval and clean
public install, update and rollback results remain separate release gates.

# Janjibar

A native Kubernetes IDE for macOS, written in Rust with
[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui). It aims to
do what [Freelens](https://github.com/freelensapp/freelens) does in one small
process, with no browser and no local proxy.

The name is a placeholder and will change.

## Status

Early. The app lists every context from your kubeconfig files, connects to
one, and shows its pods live. Nothing to install yet. See [PLAN.md](PLAN.md)
for the plan.

## Run it

You need macOS on Apple silicon, Xcode, and [Nix](https://nixos.org) with
flakes. Rust comes from the dev shell.

```sh
nix develop -c cargo run -p app --release
nix develop -c cargo run -p app --release -- --context my-context   # connect at start
```

No cluster? Run the fake one, which serves 41 pods and changes one every few
seconds:

```sh
nix develop -c cargo run -p engine --example fake_cluster -- --kubeconfig /tmp/fake.yaml
KUBECONFIG=/tmp/fake.yaml nix develop -c cargo run -p app --release -- --context fake
```

## Build the app bundle

```sh
nix develop -c script/bundle-mac
open target/dist/*.app
```

This makes a whole-program optimised build (several minutes), strips it and
wraps it in a `.app` signed ad hoc. That is enough to run it on the Mac that
built it. There is no Developer ID signature or notarization yet, so
Gatekeeper blocks a copy downloaded on another Mac.

The Release workflow builds the same bundle on GitHub and zips it. Run it by
hand from the Actions tab for a test build; the zip is kept as a workflow
artifact. To publish, push a tag that matches the workspace version, like
`v0.1.0`, and the zip and its checksum are attached to that tag's release. To
open a downloaded copy, unzip it and run
`xattr -dr com.apple.quarantine <name>.app` first.

## Develop

See [AGENTS.md](AGENTS.md) for the layout, commands and rules. It is written
for coding agents and works just as well for people.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE) for bundled
third-party code, fonts and icons.

# Kubeconfig, auth and PATH

## Which files

1. Every entry in `KUBECONFIG`, split with `std::env::split_paths`, empty
   entries ignored, in order.
2. `~/.kube/config`, if it is not already in the list.

Freelens also syncs extra files and folders the user adds. That comes later
and uses the same catalog: a source list, read in order.

## Merge rules (kubectl and `Kubeconfig::merge`)

From the Kubernetes docs on organising cluster access:

- Ignore empty filenames. Missing files are skipped.
- Error on content that cannot be deserialized. We report the error per file
  and keep going with the others.
- The first file to set a value or map key wins. A second `red-user` is
  discarded entirely, even its non-conflicting fields.
- `current-context` comes from the first file that sets it.

`Kubeconfig::read_from` already rewrites relative `certificate-authority`,
`client-certificate`, `client-key`, `tokenFile` and exec `command` paths
(the last only when it contains a `/`) against the file's own directory.

## Exec auth plugins

- Spec: `users[].user.exec` with `command`, `args`, `env`, `apiVersion`,
  `interactiveMode`, `provideClusterInfo`.
- kube 5 runs the plugin in `Auth::try_from(&AuthInfo)`, called from
  `Client::try_from(Config)`. It is a plain blocking `Command::output()`.
  Token refresh later runs on `spawn_blocking` inside kube.
- Errors (in `kube::client::AuthError`):
  - `AuthExecStart(io::Error)`: could not start. `NotFound` means the binary is
    not on PATH. Tell the user which command and which PATH.
  - `AuthExecRun { cmd, status, out }`: it ran and failed. `out` holds stderr.
  - `AuthExecParse`: output was not an `ExecCredential`.
- `interactiveMode: Always` plugins want a terminal. A GUI app has none. Until
  we run plugins ourselves, report them as needing a terminal login first.

## PATH sync

GUI apps launched from Finder or the Dock inherit launchd's PATH, which lacks
Homebrew, nix profiles, asdf and friends. Freelens, VS Code and Zed all copy
the login shell's environment at startup.

How the app does it (`crates/app/src/shell_env.rs` drives, the parser lives
in `engine::shell_env`):

1. Run `$SHELL -l -i -c 'printf <start>; env -0; printf <end>'` with stdin
   from `/dev/null` and a timeout (5 s). Interactive mode is needed because
   many users set PATH in `.zshrc`.
2. Take the bytes between the markers (shells may print banners before), split
   on NUL, split each entry at the first `=`.
3. Copy `PATH` (only PATH for now) into the process environment before the
   tokio runtime or any other thread starts. `std::env::set_var` is `unsafe`
   in edition 2024 because other threads may read the environment at the same
   time; at that point there are none.
4. On timeout or failure, keep the inherited PATH and log a warning.

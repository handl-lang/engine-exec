# exec

`hnl-exec` — the abstraction crate for the eval operation.

An `exec-*` crate: the trait/type surface for evaluation that concrete
engines (`exec-tree-walk`, `exec-bytecode`,
`exec-cranelift`) implement.

## Development

```bash
cargo run --manifest-path xtask/Cargo.toml -- gate   # fmt → clippy → test → doc
```

The repo's `xtask/` crate provides the gate tasks (`fmt`, `clippy`, `test`, `doc`,
`check`, `gate`); `ci.sh` is the gate entry. Deps resolve from git, so no
sibling checkouts are needed; a plain clone test-runs as-is.

## License

MIT.

## Documentation

<https://handl-lang.github.io/docs/exec/>

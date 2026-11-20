# engine-exec

`hnl-engine-exec` — the abstraction crate for the eval operation.

An `engine-*` crate: the trait/type surface for evaluation that concrete
engines (`engine-exec-tree-walk`, `engine-exec-bytecode`,
`engine-exec-cranelift`) implement.

## Development

```bash
cargo xtask gate        # fmt --check → clippy -D warnings → test → doc
```

`cargo xtask` wraps the repo's `xtask/` crate (`fmt`, `clippy`, `test`, `doc`,
`check`, `gate`); `ci.sh` is the gate entry. Deps resolve from git, so no
sibling checkouts are needed; a plain clone test-runs as-is.

## License

MIT.

## Documentation

<https://handl-lang.github.io/docs/engine-exec/>

# PolyTread

This package installs the native open-source PolyTread binary and exposes it as the global
`polytread` command.

```sh
npm install --global polytread
polytread
```

On first launch, PolyTread opens a terminal setup wizard with hidden credential input. The
private key is stored in the operating-system credential vault, not in NPM, JavaScript,
the dashboard, or a plaintext config file.

The native service prints a rotating localhost dashboard access link. Open that exact link after
each restart; it establishes a browser session without persisting the access token.

Stop a running local service from another terminal:

```sh
polytread shutdown
```

See the [source repository](https://github.com/EH-a0/polytread) for implementation details, build
instructions, and licenses.

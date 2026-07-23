# WASM execution-sandbox fixtures (Story 17.3a)

Adversarial-by-construction WebAssembly **component** fixtures that prove the
`ExecutionSandbox` / `WasmIsolationBackend` really contains untrusted guests
(party ruling F3: the proving consumer is a real invocation, never a mocked
`Err`).

## Contract

Every fixture is a component exporting:

```wit
run: func(input: list<u8>) -> u32
```

The backend lowers the complete `req.input` byte vector through the component
ABI and encodes the returned `u32` into `SandboxOutcome.output` (4
little-endian bytes). Some fixtures import the
existence-boolean secret probe:

```wit
has-credential: func() -> bool   // never returns the secret VALUE (N1)
```

## Fixtures

| File | Behaviour | Proves |
|---|---|---|
| `well_behaved` | returns the input-byte sum | same-length payload contents cross the component boundary |
| `infinite_loop` | spins forever | fuel, epoch, and cancellation traps remain bounded |
| `memory_bomb` | declares a ~65 MiB memory | initial linear-memory cap trap |
| `memory_grow_then_trap` | handles denied growth, then executes `unreachable` | later guest traps are not misclassified as memory traps |
| `table_bomb` | declares a 10M-element table | table allocation cannot bypass host-memory containment |
| `ungranted_import` | imports `forbidden-egress` | custom host imports are deny-by-default |
| `ungranted_wasi_random` | imports `wasi:random/random` | an empty grant exposes no implicit host WASI clock/RNG surface |
| `secret_read` | returns `has-credential()` | existence-boolean only, no value leak |
| `fuel_ok` | 100-iteration countdown | fuel boundary, low side |
| `fuel_bomb` | 100 000-iteration countdown | fuel boundary, high side |

## Build

Source of truth is `src/*.wat`. Rebuild the `.wasm` bytes and refresh the
sha256 pins in `manifest.json`:

```sh
./build.sh
```

Requires [`wasm-tools`](https://github.com/bytecodealliance/wasm-tools)
(`cargo install wasm-tools`; verified with 1.252.0). **No wasm toolchain runs in
CI** — the test asserts the committed `manifest.json` hashes and executes the
committed bytes. A tampered/swapped `.wasm` fails the hash assertion RED.

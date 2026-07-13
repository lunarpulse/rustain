;; Fixture: ungranted-import guest (Story 17.3a, AC2 mutant c).
;; Imports `forbidden-egress`, a capability OUTSIDE the sandbox's host
;; vocabulary. The per-call linker only ever defines granted host imports, so
;; this import is never satisfied and the guest FAILS TO INSTANTIATE
;; (deny-by-default at the door). Maps to TrapKind::UngrantedImport.
(component
  (import "forbidden-egress" (func $fe (result u32)))
  (core func $fe_core (canon lower (func $fe)))
  (core module $m
    (import "host" "forbidden-egress" (func $fe (result i32)))
    (memory (export "memory") 1)
    (global $heap (mut i32) (i32.const 1024))
    (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
      global.get $heap
      local.get 3
      i32.add
      global.set $heap
      global.get $heap
      local.get 3
      i32.sub)
    (func (export "run") (param i32 i32) (result i32)
      call $fe))
  (core instance $i (instantiate $m
    (with "host" (instance (export "forbidden-egress" (func $fe_core))))))
  (alias core export $i "memory" (core memory $memory))
  (alias core export $i "cabi_realloc" (core func $realloc))
  (alias core export $i "run" (core func $run))
  (func (export "run") (param "input" (list u8)) (result u32)
    (canon lift (core func $run) (memory $memory) (realloc $realloc))))

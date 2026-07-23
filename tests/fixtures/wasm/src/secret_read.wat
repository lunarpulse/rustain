;; Fixture: secret-read guest (Story 17.3a, AC2 mutant d + N1).
;; Imports the existence-boolean secret probe `has-credential() -> bool` and
;; returns its result as a u32 (0/1). The guest CANNOT obtain a secret *value*:
;; the host vocabulary has no byte-returning secret accessor at all.
(component
  (import "has-credential" (func $hc (result bool)))
  (core func $hc_core (canon lower (func $hc)))
  (core module $m
    (import "host" "has-credential" (func $hc (result i32)))
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
      call $hc))
  (core instance $i (instantiate $m
    (with "host" (instance (export "has-credential" (func $hc_core))))))
  (alias core export $i "memory" (core memory $memory))
  (alias core export $i "cabi_realloc" (core func $realloc))
  (alias core export $i "run" (core func $run))
  (func (export "run") (param "input" (list u8)) (result u32)
    (canon lift (core func $run) (memory $memory) (realloc $realloc))))

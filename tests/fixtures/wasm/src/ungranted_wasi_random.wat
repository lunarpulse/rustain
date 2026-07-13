;; Fixture: ungranted WASI-random guest (Story 17.3a review hardening).
;; Merely importing `wasi:random/random` must fail to instantiate under an empty
;; grant. The guest never needs to call it: structural absence from the linker
;; is the capability boundary.
(component
  (type $random (instance
    (type $bytes (list u8))
    (export "get-random-bytes" (func (param "len" u64) (result $bytes)))
    (export "get-random-u64" (func (result u64)))))
  (import "wasi:random/random@0.2.12" (instance $random (type $random)))
  (core module $m
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
      i32.const 0))
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $memory))
  (alias core export $i "cabi_realloc" (core func $realloc))
  (alias core export $i "run" (core func $run))
  (func (export "run") (param "input" (list u8)) (result u32)
    (canon lift (core func $run) (memory $memory) (realloc $realloc))))

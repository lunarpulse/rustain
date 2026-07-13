;; Fixture: infinite-loop guest (Story 17.3a, AC2 mutant a).
;; `run` spins forever. Every wasm instruction consumes fuel, so a finite fuel
;; cap traps it (OutOfFuel) within a bounded number of steps — never a hang,
;; never a host stall. Imports nothing.
(component
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
      (loop $l (br $l))
      unreachable))
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $memory))
  (alias core export $i "cabi_realloc" (core func $realloc))
  (alias core export $i "run" (core func $run))
  (func (export "run") (param "input" (list u8)) (result u32)
    (canon lift (core func $run) (memory $memory) (realloc $realloc))))

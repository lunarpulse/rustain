;; Fixture: fuel-at-cap+1 guest (Story 17.3a, AC3 boundary pair, high side).
;; Same shape as `fuel_ok` but a 100_000-iteration countdown — needs far more
;; fuel than `fuel_ok`'s consumption `C`.
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
    (func (export "run") (param i32) (param $len i32) (result i32)
      (local $i i32)
      (local.set $i (i32.const 100000))
      (loop $l
        (local.set $i (i32.sub (local.get $i) (i32.const 1)))
        (br_if $l (local.get $i)))
      local.get $len))
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $memory))
  (alias core export $i "cabi_realloc" (core func $realloc))
  (alias core export $i "run" (core func $run))
  (func (export "run") (param "input" (list u8)) (result u32)
    (canon lift (core func $run) (memory $memory) (realloc $realloc))))

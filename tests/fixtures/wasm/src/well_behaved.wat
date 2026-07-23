;; Fixture: well-behaved guest (Story 17.3a, AC2/AC3).
;; Exports `run(input: list<u8>) -> u32`, returning the byte sum. Two
;; same-length inputs with different content therefore prove that the payload,
;; not merely its length, crosses the component boundary. Imports nothing.
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
    (func (export "run") (param $ptr i32) (param $len i32) (result i32)
      (local $i i32)
      (local $sum i32)
      (block $done
        (loop $loop
          local.get $i
          local.get $len
          i32.ge_u
          br_if $done
          local.get $sum
          local.get $ptr
          local.get $i
          i32.add
          i32.load8_u
          i32.add
          local.set $sum
          local.get $i
          i32.const 1
          i32.add
          local.set $i
          br $loop))
      local.get $sum))
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $memory))
  (alias core export $i "cabi_realloc" (core func $realloc))
  (alias core export $i "run" (core func $run))
  (func (export "run") (param "input" (list u8)) (result u32)
    (canon lift (core func $run) (memory $memory) (realloc $realloc))))

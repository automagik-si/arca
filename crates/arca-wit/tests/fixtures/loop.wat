;; A component whose exported `run` never returns. Its only effect is a loop, so a fuel limit is the one thing that
;; can stop it.
(component
  (core module $loop
    (func (export "run")
      (loop $forever
        (br $forever))))
  (core instance $loop-instance (instantiate $loop))
  (type $run-type (func))
  (func (export "run") (type $run-type)
    (canon lift (core func $loop-instance "run"))))

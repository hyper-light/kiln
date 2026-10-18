(ns fixture.core-test
  (:require [clojure.test :refer [deftest is]]
            [fixture.core :refer [add]]))

(deftest adds
  (is (= 3 (add 1 2))))

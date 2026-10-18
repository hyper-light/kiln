module Main (main) where

import Fixture (add)
import System.Exit (exitFailure)

main :: IO ()
main = if add 1 2 == 3 then putStrLn "adds" else exitFailure

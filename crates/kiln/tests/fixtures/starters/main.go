package main

import "fmt"

// Add adds two numbers.
func Add(a, b int) int { return a + b }

func main() { fmt.Println("fixture", Add(1, 2)) }

ThisBuild / scalaVersion := "3.3.4"

lazy val root = (project in file("."))
  .settings(
    name := "fixture",
    Compile / scalaSource := baseDirectory.value / "scala" / "main",
    Test / scalaSource := baseDirectory.value / "scala" / "test",
    // Maven's JUnit tests are Maven's and Gradle's to run.
    Test / javaSource := baseDirectory.value / "scala" / "none",
    libraryDependencies += "org.scalameta" %% "munit" % "1.0.3" % Test
  )

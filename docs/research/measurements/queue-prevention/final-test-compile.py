import os,runpy
os.chdir("/private/tmp/kiln-queue-prevention-20261111")
os.environ["KILN_CHECK_MODE"]="test"
os.environ["KILN_CHECK_SCRATCH_GIB"]="8"
runpy.run_path("docs/research/measurements/queue-prevention/combined-check.py",run_name="__main__")

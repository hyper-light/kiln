-- U01: a stopped grant's exact authenticated receipt survives resource-tuple retirement.
CREATE TABLE ci_executor_receipts (
    executor TEXT NOT NULL,
    generation TEXT NOT NULL,
    allocation TEXT NOT NULL,
    agent TEXT NOT NULL,
    physical_execution TEXT NOT NULL,
    PRIMARY KEY (executor, generation, allocation, agent, physical_execution)
);

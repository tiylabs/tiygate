-- SQLite INTEGER is already a signed 64-bit value, so no schema change is
-- needed for the token/cost counters. Keep migration versions aligned with
-- PostgreSQL and record the compatibility step.
SELECT 1;

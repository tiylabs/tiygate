-- Preserve the normalized typed requirement leaves used to calculate a
-- capability shape hash.  Empty values keep rows created by older binaries
-- backwards compatible; the store reconstructs boolean requirements from
-- required_capabilities_json when reading them.
ALTER TABLE capability_route_admissions
    ADD COLUMN required_requirements_json TEXT NOT NULL DEFAULT '[]';

-- Add migration script here
CREATE TABLE payloads (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    timestamp TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    source VARCHAR(255),
    synthetic VARCHAR(255),
    synthetic_numerator VARCHAR(1024),
    synthetic_denominator VARCHAR(1024),
    validity_lower BIGINT UNSIGNED,
    validity_upper BIGINT UNSIGNED,
    payload BLOB,
    error_text VARCHAR(8192)
);

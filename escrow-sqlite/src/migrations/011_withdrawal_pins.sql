-- A seller's withdrawal PIN: asked for on every withdrawal, so a stolen sign-in alone cannot empty a
-- balance. Only an Argon2 hash is kept. Wrong guesses are counted, and too many lock it for a while.
CREATE TABLE withdrawal_pins (
    user_id      TEXT    PRIMARY KEY,
    pin_hash     TEXT    NOT NULL,
    failures     INTEGER NOT NULL DEFAULT 0,
    locked_until INTEGER,
    updated_at   INTEGER NOT NULL
);

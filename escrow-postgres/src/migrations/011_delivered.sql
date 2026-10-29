-- When the seller said the goods or work were delivered (a note of kind "delivered"). Empty until
-- they do. It changes no state and moves no money; it lets the buyer's dashboard ask for their move.
ALTER TABLE escrows ADD COLUMN delivered_at BIGINT;

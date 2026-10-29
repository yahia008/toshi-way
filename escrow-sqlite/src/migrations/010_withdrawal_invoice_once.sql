-- A withdrawal's invoice is paid at most once, ever. Before, a failed withdrawal freed its invoice
-- to be tried again; but a payment the node lost track of may still complete, and then the same
-- invoice would be paid twice.
DROP INDEX withdrawals_live_hash;
CREATE UNIQUE INDEX withdrawals_hash ON withdrawals (payment_hash);

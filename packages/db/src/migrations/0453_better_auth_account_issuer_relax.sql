-- Better Auth 1.7.3 removed the `issuer` requirement that 1.7.0-1.7.2 added
-- (migration 0230): accounts are recognized by `provider_id` and
-- `account_id` again, and 1.7.3+ schema validation rejects a required
-- column the adapter never writes, which fails every account insert and
-- blocks sign-up and session lookups. Per the 1.7 upgrade guide
-- ("Account identity keeps the provider key"), drop the compound unique
-- index first, then relax the column. Existing rows keep their backfilled
-- issuer values; new rows store NULL.
DROP INDEX IF EXISTS "account_issuer_account_id_uq";--> statement-breakpoint
ALTER TABLE "account" ALTER COLUMN "issuer" DROP NOT NULL;

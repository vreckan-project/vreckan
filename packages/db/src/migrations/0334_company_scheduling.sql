ALTER TABLE "companies" ADD COLUMN IF NOT EXISTS "scheduling" jsonb DEFAULT '{}'::jsonb NOT NULL;

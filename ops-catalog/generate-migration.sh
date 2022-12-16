#!/bin/bash

# This script generates a SQL migration, which updates the ops catalog template
# and then creates a new publication of the ops catalog for every existing tenant.
# The intended usage for each environment is:
# Locally: ./ops-catalog/generate-migration.sh local | psql 'postgresql://postgres:postgres@localhost:5432/postgres'
# In production: ./ops-catalog/generate-migration.sh prod | psql <prod-postgres-url>
# The required positional argument identifies the specific flow.yaml file to bundle as the template.
# This will be either `template-local.flow.yaml` or `template-prod.flow.yaml`.


set -e

ENVIRONMENT="$1";
if [[ -z "$ENVIRONMENT" ]]; then
	echo "missing required positional argument of 'prod' or 'local'" 1>&2
	exit 1
fi

SCRIPT_DIR=$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )
INPUT_FILENAME="${SCRIPT_DIR}/template-${ENVIRONMENT}.flow.yaml"
# Run the bundled catalog through sed to escape any single quotes that may be present.
# For postgres, this is done by doubling the single quote character (replace ' with '').
BUNDLED_CATALOG="$(flowctl raw bundle --source "$INPUT_FILENAME" | sed "s/'/''/g")"

if [[ "$?" != 0 ]]; then
	echo "Failed to bundle ops catalog" 1>&2
	exit 1
fi

cat << EOF
-- This migration was generated by ops-catalog/generate-migration.sh
-- It updates the ops catalog template that's used by agent when provisioning new tenants,
-- and also re-publishes the ops catalogs for all existing tenants.
begin;
select internal.update_ops_catalog_template('$BUNDLED_CATALOG');

select internal.republish_all_ops_catalogs((select id from auth.users where email = 'support@estuary.dev' limit 1));
commit;
EOF


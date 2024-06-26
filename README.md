# Introduction

## airbyte-to-flow

The way we use airbyte connectors is:
- We have a component called `airbyte-to-flow`, which you can find [here](https://github.com/estuary/airbyte/tree/master/airbyte-to-flow)
- `airbyte-to-flow` basically translates between the Airbyte protocol to Flow protocol, since the protocols are different
- For us to be able to use Airbyte connectors, we create docker images which are based on Airbyte connector docker images, and then we add `airbyte-to-flow` to those docker images, and run the connector through `airbyte-to-flow`. This allows us to run these new docker images in our system. You can find an example of such a [Dockerfile here](https://github.com/estuary/airbyte/blob/master/airbyte-integrations/connectors/source-salesforce/Dockerfile).
- The Dockerfiles are rather simple, they are based on the airbyte image, pull in `airbyte-to-flow`, and add some JSON files to the docker image (for the purpose of Patching, which you can read about in our README), and add some labels which help Flow determine what kind of protocol this connector speaks (in case of airbyte connectors, they use flow json capture protocol)

## Adding a new connector

1. First, find the latest tag of a connector. To do so, find your connector in airbyte repository [here](https://github.com/airbytehq/airbyte/blob/master/airbyte-integrations/connectors) and look at the `Dockerfile` of the connector, there will be a `io.airbyte.version` LABEL at the bottom of the `Dockerfile`, use the value of this label exactly as it is.

2. Then, run `new-connector.sh` script with two arguments: the connector name and the version tag. Example: `./new-connector.sh freshdesk 0.1.0`. This will create a new directory `airbyte-integrations/connectors/source-freshdesk`.

3. Now you can build a local docker container of this connector so you can run it using `flowctl` and `flowctl-go` to interact with it. To build the connector, run:
```
./local-build.sh source-freshdesk
```

4. Once you have the connector built, there are a few things that will need to be checked and updated as necessary:

#### Checking the rendering of the form on the UI

To check how the form for this connector would render in the UI, you will need to:
1. Add it to the `connectors` table on supabase
2. Visit the [form test page](https://dashboard.estuary.dev/test/jsonforms) and choose your connector from the list at the top
3. Take the connector config schema and paste it there, to do so, run the following command and copy the output:

```
flowctl-go api spec --image ghcr.io/estuary/source-freshdesk:local | jq '.configSchema'
```

Depending on how the form renders, you may want to patch certain things on the config specification schema, to do so you can use the `spec.patch.json` file. See [Patching](#patching) section below for more details.

#### Checking the schema of different connector streams (collection schemas)

It is important to check the schema of different collections of the connector. To do so, we will need to first discover the connector and then run it to ensure we can actually capture documents without document schema violations.

To discover the connector, run the following command:

```
flowctl raw discover ghcr.io/estuary/source-freshdesk:local
```

After running this command once, you should be able to find some files added to your environment, namely, `source-freshdesk.flow.yaml` and `acmeCo/flow.yaml` files. You should now open the `acmeCo/flow.yaml` file and fill in the `config` section with appropriate values that would allow you to capture data with this connector.

Once you have the configuration filled in, you can run the discover command once more to actually discover the collections of this connector:

```
flowctl raw discover ghcr.io/estuary/source-freshdesk:local
```

If the process has been successful, you should see more files under `acmeCo/` for schema of different collections. Now that we have the capture discovered, we can try running it for a while to see if we hit any issues with schema violations, etc.

To run the connector, use the `flowctl raw capture` command, with the `*.flow.yaml` file generated from the previous steps as its argument:

```
flowctl raw capture source-freshdesk.flow.yaml
```

This command will attempt to run the connector for some time and output documents to you. Check the documents and make sure there are no errors.

If there are errors about schema violations, we need to patch the stream schemas to arrive at a schema that is not violated by the documents anymore. See [Patching](#patching) section below for more.

## Patching

These files can be placed in the root directory of the connector
and copied in Dockerfile. The following files are supported:

1. `spec.patch.json`: to patch the connector's endpoint_spec, the patch is applied as specified by [RFC7396 JSON Merge](https://www.rfc-editor.org/rfc/rfc7396.txt)
2. `spec.map.json`: to map fields from endpoint_spec. Keys and values are JSON pointers. Each key: value in this file is processed by moving whatever is at the value pointer to the key pointer
3. `oauth2.patch.json`: to patch the connector's oauth2 spec. This patch overrides the connector's oauth2 spec
4. `documentation_url.patch.json`: to patch the connector's
   documentation_url. Expects a single key `documentation_url` with a string value
5. `run_interval_minutes.json`: to set a run interval for connector. The value of this file should be a number that indicates how many minutes should runs of the connector be spaced. For example, a value of 30 means that the connector will not run successfully more frequently than once every 30 minutes (if the connector errors, it will be restarted immediately).
5. `streams/<stream-name>.patch.json`: to patch a specific stream's document schema
6. `streams/<stream-name>.pk.json`: to patch a specific stream's primary key, expects an array of strings
7. `streams/<stream-name>.normalize.json`: to apply data normalization functions to specific fields of documents generated by a capture stream. Normalizations are provided as a list (JSON array) of objects with keys `pointer` having a value of the pointer to the document field that the normalization will apply to, and `normalization` having a value of the name of the normalization function to apply to the field at that pointer. Normalization function names should provided as `snake_case`; see `airbyte_to_flow/src/interceptors/normalize.rs` for the supported normalization functions.

After updating patch files, make sure you build the connector again before running commands to test it, a re-build is necessary to include your patch changes.

# Using `flowctl raw suggest-schema` on tasks with document schema violations

We do not always have the correct schema for collections, and we end up with document schema violations when we get a new document that somehow does not fit our existing schema. Moreover, sometimes we do not have any information on some of the properties that are available in the collection (we are missing some properties).

In both of these instances, it helps to use `flowctl raw suggest-schema` command to find a better-suited schema for our collection. This command works by reading all the documents of a collection, as well as documents that have violated the existing schema (those documents are read from the ops log) and running schema-inference on all of these documents, to come up with a new schema. The command then runs a diff between the two schemas and the original schema as well as the inferred schema are also available. By reading the diff you can find out which fields need updating, and the suggested approach is to take those fields from the `new.schema.json` file produced by the command.

As an example, let's say a task called `acmeCo/data/source-x` has failed
with some document schema violations in its collection `acmeCo/data/anvils`, in order to find a better-suited schema,
you can run the command:

```
flowctl raw suggest-schema --collection acmeCo/data/anvils --task acmeCo/data/source-x
```

The command will run for a while, and will write two files
`anvils.original.schema.json` and `anvils.new.schema.json`, which are the
original schema of the collection and the new inferred schema, respectively. You
will also see the diff run on these two files. If you want to run such a diff
yourself, you can use (we use `git` because it has better output formatting, but
this command does not need to be run in a git repository):

```
git diff --no-index anvils.original.schema.json anvils.new.schema.json
```

Using the information from this command, you can write a [patch file](#patching)
to fix the document schema.

Note that sometimes this command will extend the type of a value with the type
of the newly-seen value from the document that violated the schema, and
sometimes this extension of the type makes sense, whereas some other times it
does not really make sense. Some common pitfalls:

- If a field has wrongly been specified as `type: array` in the original schema,
  but it is actually an object, the new inferred schema will have `type:
  ["array", "object"]`, which is probably not the tightest schema we can have.
  It is good to check whether other documents in the collection have the type
  `array` at all, or if it is just a mistake in the schema. If it is a mistake,
  then it is preferred to have `type: object`.
- If a field is missing from the existing schema of a collection, but the value
  does come through from the capture, sometimes the value will always have the
  value `null`, and as such the suggested schema will be `type: null`. This is
  not an intuitive schema specification, but it is okay to go ahead and apply
  this patch: this will allow us to get a notification once we see a non-null
  value for this property, and at that point we can fix it by running
  schema-inference again and get the correct schema. However, this may be time-consuming until we have
  automatic continuous schema-inference running on these collections, so it is also a
  fair choice to omit these fields from your patch.
- Do not include `_meta` field changes in patches


# CI/CD

To build images for this new connector, you need to add this connector name
   to `.github/workflows/connectors.yml`,
   `jobs.build_connectors.strategy.matrix.connector` is the place to add the new
   connector.

## Updating an existing connector (no code)

To update a connector, open the `Dockerfile` of that connector, and in the `FROM airbyte/source-${NAME}:${VERSION}` line, replace `VERSION` with the latest tag of the connector.

# Advanced

## Local Flow testing
To test the connector in your local flow setup, you can push the connector with
a personal tag. First you need to login docker to `ghcr.io`, to do this you need
to create a [github personal
access
token](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/creating-a-personal-access-token)
with access to push to our container registry. Once you have this token, you can
login using the command below:

```
echo '<YOUR-TOKEN>' | docker login ghcr.io -u <YOUR-GITHUB-USERNAME> --password-stdin
```

Then you can push the connector by passing `-p` to `local.build.sh`. This
command will push the docker container with a tag unique to your codespace which will
be printed at the end.

```
./local-build.sh -p source-intercom
```

## Updating an existing connector (connectors with code)

The `pull-connector.sh` script can update existing connectors as well. You can
just run:

```
./pull-connector.sh source-hubspot
```

The script will take you through a diff of the latest version from airbyte and
our local version, and will ask you about each file whether we should keep the
local file or take the file from upstream. A rule of thumb is that we want to
pull in code changes, but we usually keep our local version of `Dockerfile` and
`.dockerignore`. Don't forget to bump the version in `Dockerfile` if the changes
we are pulling from upstream are backward-incompatible.

## airbyte-to-flow

See the README file in `airbyte-to-flow` directory.

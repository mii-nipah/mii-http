# mii-http

An easy way for running an HTTP server for a shell utility.

You define the specs for the server in a .http file, and then you can run the server with the `mii-http` command.

## .http file

You may define endpoints, the types, query parameters and body, the response and also the expected executions.

Comments are allowed in the .http file, and they start with `#`.

### Setup
Every specs file should start with a nice setup section, where you can define your version, authentication method and other general settings for your server.
```http
# this is the version of your specs, which will translate to a /vN endpoint, where N is the version number
VERSION 1

# you may also define a base path for your endpoints, for example in the case below it's /named, which means that, for example, supposing you have an endpoint defined as GET /stats, it will be available at /named/v1/stats
BASE /named

# here you define the authentication method for your specs, currently only bearer tokens are supported
# [HEADER T] implies that the server shall seek the token in the header of the request, where T is the name of the header, could be for example "Server_Api_Token" or something similar
AUTH Bearer [HEADER API_TOKEN]

# we also have partial support for JWT specifically, so you can specify the verifier for the bearer token (if you specify one, the bearer token will be considered to be a JWT)
# Server_Jwt_Verifier will be obtained from the available environment variables of the process
JWT_VERIFIER [ENV Server_Jwt_Verifier]

# if you don't provide a JWT_VERIFIER, you will need to provide the TOKEN_SECRET, which will be used to verify the received one is correct
TOKEN_SECRET [ENV Server_Token_Secret]

# you can define the max body size for your endpoints, for example in the case below it's 1mb
MAX_BODY_SIZE 1mb

# you can also define the maximum acceptable size for query parameters, for example in the case below it's 100 characters
MAX_QUERY_PARAM_SIZE 100

# you can also define the maximum acceptable size for headers, for example in the case below it's 100 characters
MAX_HEADER_SIZE 100

# you can also define a timeout for your endpoints, for example in the case below it's 30 seconds
TIMEOUT 30s
```

### Endpoint definition
After the setup section, you can start defining your endpoints. An endpoint is defined by its method (GET, POST, PUT, DELETE), its path and its specs.
```http
# the method of your endpoint + the path, which is relative to the other components already defined such as BASE and VERSION
GET /status
# defines the response type of your endpoint
Response-Type text/plain
# defines the command that mii-http will execute when this endpoint is called
Exec: mii-sound --status
```

You may define query parameters and proper body for your endpoint, for example:
```http
POST /greet
Response-Type text/plain
# defines a query parameter called "name", which is required and of type string
QUERY name: /[a-zA-Z0-9_]+/
# optional query parameter
QUERY guest?: /[a-zA-Z0-9_]+/
# defines the body of the request, which is expected to be a simple json without any specific schema
BODY json
Exec: echo $ | xargs echo "Hello, {%name}!"
```

All headers besides the AUTH one need to be properly defined in the specs file, for example:
```http
GET /headers
Response-Type text/plain
# defines a header called "X-Custom-Header", which is required and of type string
HEADER X-Custom-Header: /[a-zA-Z0-9_]+/
# optional header
HEADER X-Optional-Header?: /[a-zA-Z0-9_]+/
Exec: echo "The value of X-Custom-Header is: {^X-Custom-Header} and the value of X-Optional-Header is: {^X-Optional-Header}"
```

And path params are defined with the syntax `:param_name` in the path, for example:
```http
GET /users/:user_id:uuid
Response-Type text/plain
# defines a path parameter called "user_id", which is required and of type uuid
Exec: echo "The user id is: {:user_id}"
```

Your BODY can also be a FORM, for example:
```http
POST /submit-form
Response-Type text/plain
BODY form {
  # defines a field in the form called "username", which is required and of type string
  username: /[a-zA-Z0-9_]+/
  # defines a field in the form called "age", which is optional and of type int
  age?: int
}
Exec: echo "The username is: {$.username} and the age is: {$.age}"
```

A BODY may also contain JSON with a specific schema, for example:
```http
POST /submit-json
Response-Type text/plain
BODY json {
  # defines a field in the json body called "title", which is required and of type string
  title: /[a-zA-Z0-9_ ]+/
  # defines a field in the json body called "count", which is optional and of type int
  count?: int,
  # defines a field in the json body called "tags", which is optional and is an array of strings
  tags?: [string]
}
```

#### Exec
The `Exec` field is a very important and specially careful aspect of your endpoint definition, as it defines a command that will be run in your system. The command will be executed in a shell, and to ensure the maximum security it will be properly sanitized and validated, and unsafe values must be intentional and conscious.

Commands have a nice syntax for interacting with the request, for example suppose you have a query param called "name", you can use it in your command like this:
```http
Exec: echo "Hello, {%name}!"
```
In the example above you can see the string interpolation syntax, which is `{value}`, what will be inside depends on your desired value.
While using a query param uses `%name`, using a path param uses `:name`, using a header param uses `^name`, and using a body param uses a simplified JSON path syntax (or just `$` if you want to use the entire body, or if your body is not a JSON but a simple string). The JSON path syntax also can be used when your BODY is a FORM for simple field access.
You may also define arbitrary variables available to your command, for example if you need to consume an environment variable there:
```http
GET /echo
Response-Type text/plain
VAR server_name [ENV Server_Name]
Exec: echo "Hello, {@server_name}!"
```
In the example above, we defined a variable called server_name, which is obtained from the environment variables of the process, and then we used it in our command with the syntax `{@name}`.

Outside of string interpolation, you can also use the same syntax for flags and positional arguments in your command, for example:
```http
GET /greet
Response-Type text/plain
VAR greeting [ENV Greeting]
Exec: echo [@greeting] [%name]

GET /flags
Response-Type text/plain
Exec: some_command [--flag %query_param]
```
Values are "interpolated" in the command when they are inside `[]`.

You can pass some value as stdin by doing the following:
```http
POST /echo
Response-Type text/plain
Exec: $ | xargs echo
```
Exec will interpret direct references to body, path, params, headers, values in general being piped as stdin. It's a special syntax for convenience, not simple text substitution.

String interpolations whose values are not defined (i.e. optionals) will be replaced with an empty string.
Flags and positional arguments whose values are not defined will be omitted entirely.

#### Types
Query parameters and body schemas can have types, they are:
- int: an integer number
- float: a floating point number
- boolean: a boolean value, which can be true or false
- uuid: a uuid string, which must be a valid uuid
- int(1..10): integer range, which allows only integer numbers between 1 and 10 (inclusive)
- float(0.0..1.0): floating point range, which allows only floating point numbers between 0.0 and 1.0 (inclusive)
- textA|textB|textC|...: a union, which means that the value can be one of the specified strings, for example `textA|textB|textC` means that the value can be either "textA", "textB" or "textC"
- /regex-here/: a regex, which means that the value must match the specified regex, for example `/^[a-zA-Z0-9_]+$/` means that the value can only contain alphanumeric characters and underscores. While it's possible to use a regex in any place, it's worth nothing that it's inherently riskier than the other types, so it's usage is not recommended if you can express your needs with the other types. Regexes are full-matching by default, so you don't need to add `^` and `$` at the beginning and end of your regex, but you can if you want to be more explicit if you want to.
- string: any string value, it can only be used for stdin to avoid the risk of command injection
- json: any json value, it can only be used for stdin to avoid the risk of command injection
- typed json: a json value that must conform to a specific schema defined in the specs, it can only be passed entirely to a command (outside of stdin) if all fields are also valid fields that accept this, and JSON path accesses are allowed with the same general rules
- binary: any binary value, it can only be define for BODY, and will be passed to the command as a temporary file reference (or, if it's stdin, directly)
- form: can only be used with BODY, and makes the server expect fields to be defined

## commands

* `mii-http --check <path>` checks your specs for errors and inconsistencies, it also flags potential security issues (such as using /.*/ on regexes and similar things)
* `mii-http <path>` runs the server with the specified specs file

* `mii-http --addr <address> <path>` runs the server with the specified specs file and binds it to the specified address, for example `mii-http --addr 0.0.0.0:8080 specs.http` will bind the server to all available interfaces on port 8080

* `mii-http --quiet` runs the server in quiet mode, which means that it will not log any requests or errors to the console, it can be useful for production environments or whatever

* `mii-http --dry-run` runs the server in dry run mode, which means that it will not actually execute the commands defined in the specs file, instead will log the commands that would be executed for each request, it can be useful for testing and debugging your specs file without actually running any command

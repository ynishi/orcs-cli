--[[
  .codedash.lua — Project configuration for codedash

  extends: inherit bindings from a preset ("recommended")
  domains: group nodes by file path patterns (vertical slice)
  layers: group domains by architectural role (horizontal slice)
]]

return {
  extends = "recommended",

  -- Auto-detected from Cargo.toml workspace
  domains = {
    { name = "orcs-types", patterns = { "^crates/orcs-types/" } },
    { name = "orcs-event", patterns = { "^crates/orcs-event/" } },
    { name = "orcs-auth", patterns = { "^crates/orcs-auth/" } },
    { name = "orcs-component", patterns = { "^crates/orcs-component/" } },
    { name = "orcs-hook", patterns = { "^crates/orcs-hook/" } },
    { name = "orcs-runtime", patterns = { "^crates/orcs-runtime/" } },
    { name = "orcs-lua", patterns = { "^crates/orcs-lua/" } },
    { name = "orcs-mcp", patterns = { "^crates/orcs-mcp/" } },
    { name = "orcs-app", patterns = { "^crates/orcs-app/" } },
    { name = "orcs-cli", patterns = { "^crates/orcs-cli/" } },
    { name = "orcs-lint", patterns = { "^crates/orcs-lint/" } },
  },

  -- Layers: group domains by architectural role
  layers = {
    { name = "Plugin SDK", domains = { "orcs-types", "orcs-event", "orcs-auth", "orcs-component" } },
    { name = "Hook", domains = { "orcs-hook" } },
    { name = "Runtime", domains = { "orcs-runtime" } },
    { name = "Plugin", domains = { "orcs-lua", "orcs-mcp" } },
    { name = "Application", domains = { "orcs-app" } },
    { name = "Frontend", domains = { "orcs-cli" } },
    { name = "Dev Tools", domains = { "orcs-lint" } },
  },
  fallback = "other",
}

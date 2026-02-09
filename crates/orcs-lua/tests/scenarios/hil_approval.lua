-- HIL (Human-in-the-Loop) Approval: Scenario Tests
--
-- Tests the approval request → pending → resolve flow
-- using a component that wraps approval logic.

local test = orcs.test

return {
    name = "HIL Approval Flow",

    component = [[
        local pending = {}
        local resolved = {}

        return {
            id = "approval-gate",
            subscriptions = {"Hil"},

            on_request = function(req)
                if req.operation == "submit" then
                    local id = req.payload.id or ("req-" .. #pending + 1)
                    pending[id] = {
                        id = id,
                        operation = req.payload.operation or "unknown",
                        description = req.payload.description or "",
                    }
                    return {
                        success = true,
                        data = { approval_id = id, status = "pending" }
                    }

                elseif req.operation == "status" then
                    local id = req.payload.id
                    if not id then
                        return { success = false, error = "id required" }
                    end
                    if resolved[id] then
                        return {
                            success = true,
                            data = { id = id, status = resolved[id] }
                        }
                    elseif pending[id] then
                        return {
                            success = true,
                            data = { id = id, status = "pending" }
                        }
                    end
                    return { success = false, error = "not found: " .. id }

                elseif req.operation == "list_pending" then
                    local items = {}
                    for id, req_data in pairs(pending) do
                        table.insert(items, req_data)
                    end
                    return { success = true, data = items }

                elseif req.operation == "pending_count" then
                    local count = 0
                    for _ in pairs(pending) do count = count + 1 end
                    return { success = true, data = { count = count } }
                end

                return { success = false, error = "unknown: " .. req.operation }
            end,

            on_signal = function(sig)
                if sig.kind == "Veto" then
                    -- Reject all pending
                    for id, _ in pairs(pending) do
                        resolved[id] = "rejected"
                    end
                    pending = {}
                    return "Abort"

                elseif sig.kind == "Approve" and sig.approval_id then
                    if pending[sig.approval_id] then
                        resolved[sig.approval_id] = "approved"
                        pending[sig.approval_id] = nil
                        return "Handled"
                    end
                    return "Ignored"

                elseif sig.kind == "Reject" and sig.approval_id then
                    if pending[sig.approval_id] then
                        resolved[sig.approval_id] = "rejected"
                        pending[sig.approval_id] = nil
                        return "Handled"
                    end
                    return "Ignored"
                end

                return "Ignored"
            end,
        }
    ]],

    scenarios = {
        -- =================================================================
        -- Submit & Query
        -- =================================================================
        {
            name = "submit creates pending request",
            run = function(h)
                local result = h:request("Hil", "submit", {
                    id = "req-1",
                    operation = "write",
                    description = "write to config.txt",
                })
                test.eq(result.approval_id, "req-1")
                test.eq(result.status, "pending")
            end,
        },
        {
            name = "status returns pending for unresolved request",
            run = function(h)
                h:request("Hil", "submit", { id = "req-1", operation = "write" })
                local status = h:request("Hil", "status", { id = "req-1" })
                test.eq(status.status, "pending")
            end,
        },
        {
            name = "status returns error for unknown id",
            run = function(h)
                local ok, err = pcall(function()
                    h:request("Hil", "status", { id = "nonexistent" })
                end)
                test.ok(not ok)
                test.contains(tostring(err), "not found")
            end,
        },
        {
            name = "pending_count tracks submissions",
            run = function(h)
                h:request("Hil", "submit", { id = "a", operation = "read" })
                h:request("Hil", "submit", { id = "b", operation = "write" })
                h:request("Hil", "submit", { id = "c", operation = "exec" })
                local result = h:request("Hil", "pending_count", {})
                test.eq(result.count, 3)
            end,
        },

        -- =================================================================
        -- Approve via Signal
        -- =================================================================
        {
            name = "approve signal resolves request",
            run = function(h)
                h:request("Hil", "submit", { id = "req-1", operation = "write" })

                local response = h:approve("req-1")
                test.eq(response, "Handled")

                local status = h:request("Hil", "status", { id = "req-1" })
                test.eq(status.status, "approved")
            end,
        },
        {
            name = "approve removes from pending",
            run = function(h)
                h:request("Hil", "submit", { id = "req-1", operation = "write" })
                h:request("Hil", "submit", { id = "req-2", operation = "read" })

                h:approve("req-1")

                local count = h:request("Hil", "pending_count", {})
                test.eq(count.count, 1, "only req-2 should remain pending")
            end,
        },
        {
            name = "approve nonexistent is ignored",
            run = function(h)
                local response = h:approve("ghost")
                test.eq(response, "Ignored")
            end,
        },

        -- =================================================================
        -- Reject via Signal
        -- =================================================================
        {
            name = "reject signal resolves with rejected",
            run = function(h)
                h:request("Hil", "submit", { id = "req-1", operation = "delete" })

                local response = h:reject("req-1", "too dangerous")
                test.eq(response, "Handled")

                local status = h:request("Hil", "status", { id = "req-1" })
                test.eq(status.status, "rejected")
            end,
        },

        -- =================================================================
        -- Veto rejects all
        -- =================================================================
        {
            name = "veto rejects all pending requests",
            run = function(h)
                h:request("Hil", "submit", { id = "a", operation = "write" })
                h:request("Hil", "submit", { id = "b", operation = "exec" })
                h:request("Hil", "submit", { id = "c", operation = "delete" })

                -- Confirm 3 pending before veto
                local count = h:request("Hil", "pending_count", {})
                test.eq(count.count, 3, "3 pending before veto")

                local response = h:veto()
                test.eq(response, "Abort")
                test.eq(h:status(), "Aborted")

                -- After veto, component is aborted and rejects requests
                local ok, err = pcall(function()
                    h:request("Hil", "pending_count", {})
                end)
                test.ok(not ok, "aborted component should reject requests")
                test.contains(tostring(err), "aborted")
            end,
        },

        -- =================================================================
        -- Mixed workflow
        -- =================================================================
        {
            name = "approve then reject different requests",
            run = function(h)
                h:request("Hil", "submit", { id = "safe", operation = "read" })
                h:request("Hil", "submit", { id = "dangerous", operation = "rm -rf" })

                h:approve("safe")
                h:reject("dangerous", "nope")

                local safe_status = h:request("Hil", "status", { id = "safe" })
                test.eq(safe_status.status, "approved")

                local danger_status = h:request("Hil", "status", { id = "dangerous" })
                test.eq(danger_status.status, "rejected")

                local count = h:request("Hil", "pending_count", {})
                test.eq(count.count, 0)
            end,
        },
    },
}

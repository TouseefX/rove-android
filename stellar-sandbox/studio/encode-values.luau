-- Bounded, data-only encoding for values returned by an untrusted Roblox script.
-- Load this module before the submitted script and keep the returned function private
-- to the trusted runner. Never put it or its captured helpers in the guest environment.
local safeType = type
local safeTypeof = typeof
local rawget = rawget
local rawNext = next
local safeSort = table.sort
local safeHuge = math.huge
local safeByte = string.byte
local safeSub = string.sub
local safeFormat = string.format
local min = math.min
local tableCreate = table.create
local tablePack = table.pack

local MAX_RETURNS = 128
local MAX_TOTAL_ENTRIES = 256
local MAX_TABLE_SCAN = 256
local MAX_TABLE_ENTRIES = 64
local MAX_DEPTH = 4
local MAX_STRING_BYTES = 1024
local MAX_TOTAL_JSON_BYTES = 24 * 1024

local function finite(value)
	return value == value and value ~= safeHuge and value ~= -safeHuge
end

local function validUtf8Prefix(text, byteLimit)
	local length = min(#text, byteLimit)
	local index = 1
	local validEnd = 0

	while index <= length do
		local first = safeByte(text, index)
		local width

		if first < 0x80 then
			width = 1
		elseif first >= 0xC2 and first <= 0xDF then
			width = 2
		elseif first >= 0xE0 and first <= 0xEF then
			width = 3
		elseif first >= 0xF0 and first <= 0xF4 then
			width = 4
		else
			break
		end

		if index + width - 1 > length then
			break
		end

		local second = width > 1 and safeByte(text, index + 1) or nil
		if width > 1 and (second < 0x80 or second > 0xBF) then
			break
		end
		if first == 0xE0 and second < 0xA0 then
			break
		end
		if first == 0xED and second > 0x9F then
			break
		end
		if first == 0xF0 and second < 0x90 then
			break
		end
		if first == 0xF4 and second > 0x8F then
			break
		end

		local valid = true
		for offset = 2, width - 1 do
			local byte = safeByte(text, index + offset - 1)
			if byte < 0x80 or byte > 0xBF then
				valid = false
				break
			end
		end
		if not valid then
			break
		end

		index += width
		validEnd = index - 1
	end

	return safeSub(text, 1, validEnd), validEnd < #text
end

local function jsonTextCost(text)
	-- Worst case every byte is an ASCII control character escaped as \u00xx.
	return #text * 6
end

local function scalarKey(value)
	local kind = safeType(value)
	if kind == "string" then
		local text, truncated = validUtf8Prefix(value, 256)
		if truncated then
			return { type = "string", value = text, truncated = true }, 1, text
		end
		return { type = "string", value = text }, 1, text
	elseif kind == "number" and finite(value) then
		return { type = "number", value = value }, 2, value
	elseif kind == "boolean" then
		return { type = "boolean", value = value }, 3, value
	end
	return nil
end

local function lessKey(left, right)
	if left.rank ~= right.rank then
		return left.rank < right.rank
	end
	if left.value == right.value then
		return false
	end
	if left.rank == 3 then
		return left.value == false
	end
	return left.value < right.value
end

local function encodeValue(value, state, depth, ancestors)
	local kind = safeType(value)
	local engineType = safeTypeof(value)
	local output = { type = engineType }

	state.entries += 1
	if state.entries > MAX_TOTAL_ENTRIES then
		state.truncated = true
		output.type = "truncated"
		output.value = "total entry limit"
		return output
	end

	if value == nil then
		state.bytes += 48
		return output
	end
	if kind == "boolean" then
		output.value = value
		state.bytes += 56
		return output
	end
	if kind == "number" then
		if finite(value) then
			output.value = value
		else
			output.value = safeFormat("%.6g", value)
		end
		state.bytes += 64
		return output
	end
	if kind == "string" then
		local text, truncated = validUtf8Prefix(value, MAX_STRING_BYTES)
		local cost = 64 + jsonTextCost(text)
		if state.bytes + cost > MAX_TOTAL_JSON_BYTES then
			text = ""
			truncated = true
			cost = 72
		end
		output.value = text
		if truncated then
			output.truncated = true
		end
		state.bytes += cost
		return output
	end
	if kind == "function" or kind == "thread" then
		output.value = "<" .. kind .. " omitted>"
		state.bytes += 96
		return output
	end
	if kind == "table" then
		if ancestors[value] then
			output.value = "<cycle>"
			state.bytes += 72
			return output
		end
		if depth >= MAX_DEPTH then
			output.value = "<depth limit>"
			state.bytes += 88
			return output
		end

		ancestors[value] = true
		local keys = tableCreate(MAX_TABLE_ENTRIES)
		local key = nil
		local scanned = 0
		local limited = false
		while true do
			key = rawNext(value, key)
			if key == nil then
				break
			end
			scanned += 1
			if scanned > MAX_TABLE_SCAN then
				limited = true
				break
			end
			local encoded, rank, sortValue = scalarKey(key)
			if encoded then
				if #keys < MAX_TABLE_ENTRIES then
					keys[#keys + 1] = { raw = key, encoded = encoded, rank = rank, value = sortValue }
				else
					limited = true
				end
			else
				limited = true
			end
		end
		safeSort(keys, lessKey)

		local entries = tableCreate(#keys)
		local emitted = 0
		for _, item in keys do
			if state.entries >= MAX_TOTAL_ENTRIES or state.bytes >= MAX_TOTAL_JSON_BYTES then
				limited = true
				break
			end
			local keyCost = 48
			if item.encoded.type == "string" then
				keyCost += jsonTextCost(item.encoded.value)
			end
			if state.bytes + keyCost > MAX_TOTAL_JSON_BYTES then
				limited = true
				break
			end
			state.bytes += keyCost
			local encodedChild = encodeValue(rawget(value, item.raw), state, depth + 1, ancestors)
			if encodedChild.type == "truncated" then
				limited = true
			end
			entries[#entries + 1] = {
				key = item.encoded,
				value = encodedChild,
			}
			emitted += 1
		end
		ancestors[value] = nil
		output.entries = entries
		if limited or emitted < scanned then
			output.truncated = true
			state.truncated = true
		end
		state.bytes += 64 + (#entries * 24)
		return output
	end

	if engineType == "Vector2" then
		output.value = { value.X, value.Y }
	elseif engineType == "Vector3" then
		output.value = { value.X, value.Y, value.Z }
	elseif engineType == "Color3" then
		output.value = { value.R, value.G, value.B }
	elseif engineType == "UDim" then
		output.value = { value.Scale, value.Offset }
	elseif engineType == "UDim2" then
		output.value = {
			{ value.X.Scale, value.X.Offset },
			{ value.Y.Scale, value.Y.Offset },
		}
	elseif engineType == "CFrame" then
		local packed = tablePack(value:GetComponents())
		local components = tableCreate(packed.n)
		for index = 1, packed.n do
			local component = rawget(packed, index)
			components[index] = finite(component) and component or nil
		end
		output.value = components
	elseif engineType == "EnumItem" then
		output.value = {
			enumType = value.EnumType.Name,
			name = value.Name,
			value = value.Value,
		}
	elseif engineType == "BrickColor" then
		output.value = { number = value.Number, name = value.Name }
	elseif engineType == "NumberRange" then
		output.value = { value.Min, value.Max }
	elseif engineType == "Rect" then
		output.value = { min = { value.Min.X, value.Min.Y }, max = { value.Max.X, value.Max.Y } }
	elseif engineType == "Ray" then
		output.value = {
			origin = { value.Origin.X, value.Origin.Y, value.Origin.Z },
			direction = { value.Direction.X, value.Direction.Y, value.Direction.Z },
		}
	elseif engineType == "Instance" then
		output.value = "<Instance omitted>"
	else
		output.value = "<" .. engineType .. " omitted>"
	end
	state.bytes += 128
	if state.bytes > MAX_TOTAL_JSON_BYTES then
		state.truncated = true
		output.value = "<output limit>"
	end
	return output
end

local function encodeReturns(packed)
	local count = rawget(packed, "n")
	if safeType(count) ~= "number" or not finite(count) or count < 0 or count % 1 ~= 0 then
		return {
			{ type = "error", value = "invalid packed return values" },
		}
	end

	local state = { bytes = 0, entries = 0, truncated = false }
	local result = tableCreate(min(count, MAX_RETURNS) + 1)
	local ancestors = {}
	local returned = 0
	for index = 1, min(count, MAX_RETURNS) do
		if state.entries >= MAX_TOTAL_ENTRIES or state.bytes >= MAX_TOTAL_JSON_BYTES then
			state.truncated = true
			break
		end
		result[#result + 1] = encodeValue(rawget(packed, index), state, 0, ancestors)
		returned += 1
	end
	if returned < count then
		result[#result + 1] = {
			type = "truncated",
			value = "return values omitted after configured limit",
		}
	elseif state.truncated then
		result[#result + 1] = {
			type = "truncated",
			value = "nested return data exceeded configured limits",
		}
	end
	return result
end

return encodeReturns

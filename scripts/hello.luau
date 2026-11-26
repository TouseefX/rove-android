print("game:", game.Name)
print("player:", game:GetService("Players").LocalPlayer.Name)
local ws = game:GetService("Workspace")
local n = 0
for _ in ipairs(ws:GetDescendants()) do n += 1 end
print("workspace descendants:", n)

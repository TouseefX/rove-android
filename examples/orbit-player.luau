local P = game:GetService("Players")
local target
for _, p in ipairs(P:GetPlayers()) do
    if p.Name:lower() == "mimiawee" then
        target = p
        break
    end
end
if target then
    print("Orbiting " .. target.Name)
    for i = 1, 400 do
        local c = P.LocalPlayer.Character
        local t = target.Character
        local r = t and t:FindFirstChild("HumanoidRootPart")
        if not c or not r then
            print("Character or root unavailable")
            break
        end
        local center = r.Position
        local pos = center + Vector3.new(math.cos(i * 0.1) * 8, 0, math.sin(i * 0.1) * 8)
        c:PivotTo(CFrame.lookAt(pos, center))
        task.wait(0.05)
    end
else
    print("GetPlayers returned no match for mimiawee")
end

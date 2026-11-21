local runService = game:GetService("RunService")
local magnitude = Vector3.new(3, 4, 0).Magnitude

print("IsClient", runService:IsClient())
print("IsStudio", runService:IsStudio())
warn("Vector magnitude", magnitude)

return magnitude, nil, workspace.Gravity, "done"

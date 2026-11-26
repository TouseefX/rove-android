local lp = game:GetService("Players").LocalPlayer
local c
for i = 1, 120 do
  c = lp.Character
  if c and c:FindFirstChild("HumanoidRootPart") then break end
  task.wait(0.25)
end
if not c then print("NOCHAR") return end
local hrp = c.HumanoidRootPart
local hum = c.Humanoid
task.wait(1.0)
local y0 = hrp.Position.Y
hum.Jump = true
local peak = 0
for i = 1, 25 do
  task.wait(0.08)
  local rise = hrp.Position.Y - y0
  if rise > peak then peak = rise end
end
print("PEAK", string.format("%.2f", peak))

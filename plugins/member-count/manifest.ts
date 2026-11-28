export default {
   description: "Shows how many members a server has, and how many are online, in its tooltip in the server list",
   version: "1.0.0",
   author: "acasamit",
   // The counts come from the client's store and Discord's API, both through
   // `phoenix.discord`.
   permissions: ["discord"],
} satisfies Phoenix.Manifest;

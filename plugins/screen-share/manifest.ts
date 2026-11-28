export default {
   description: "Restores screen share and camera in Brazil, without a VPN",
   version: "1.0.0",
   author: "Aprovei",
   warning:
      "When Discord opens on Windows, Discord traffic goes through a public proxy until the session starts. The proxy sees the destination, not the contents. Voice and screen do not go through it.",
} satisfies Phoenix.Manifest;

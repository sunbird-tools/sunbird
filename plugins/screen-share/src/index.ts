phoenix.settings.define({
  mode: {
    type: "select",
    title: "Screen share",
    description:
      "On Windows, when the session region is Brazil, startup goes through an outside address and then returns to the direct path. Camera, voice, and screen stay on UDP. Applies the next time Discord opens.",
    options: [
      { value: "auto", label: "Automatic" },
      { value: "on", label: "Always" },
      { value: "off", label: "Off" },
    ],
    default: "auto",
    onChange(value) {
      phoenix.log.info(`screen share: ${value}, applies the next time Discord opens`);
    },
  },
});

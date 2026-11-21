import { array, lenient, num, object, optional, str } from "@agent/protocol";

const member = object({ user: object({ id: str }) });

const option = object({ name: str, value: str });

/**
 * The Discord interaction fields the control plane reads. Fields Discord sends
 * that the product does not use are dropped by the decoder, and the fields
 * that vary by interaction type are optional so one shape covers PING,
 * commands, and components.
 */
export const decodeDiscordInteraction = object({
  id: optional(str),
  type: num,
  channel_id: optional(str),
  guild_id: optional(str),
  channel: optional(object({ id: str, guild_id: optional(str) })),
  member: optional(lenient(member)),
  user: optional(object({ id: str })),
  data: optional(
    object({
      custom_id: optional(str),
      options: optional(array(option)),
    }),
  ),
});

export type DiscordInteraction = ReturnType<typeof decodeDiscordInteraction>;

/** The id of the user who triggered the interaction, from either place Discord puts it. */
export function authorOf(interaction: DiscordInteraction): string {
  if (interaction.member !== undefined) return interaction.member.user.id;
  return interaction.user === undefined ? "" : interaction.user.id;
}

/** The channel id, from either field Discord puts it in. */
export function channelOf(interaction: DiscordInteraction): string | undefined {
  if (interaction.channel_id !== undefined) return interaction.channel_id;
  return interaction.channel?.id;
}

/** The guild, when the interaction happened in one. A direct message has none. */
export function guildOf(interaction: DiscordInteraction): string {
  const fromChannel = interaction.channel?.guild_id;
  if (fromChannel !== undefined) return fromChannel;
  return interaction.guild_id === undefined ? "" : interaction.guild_id;
}

/**
 * The prompt: a button's `custom_id`, or the `prompt` option of a slash
 * command. `undefined` means the interaction carries no prompt at all.
 */
export function promptOf(interaction: DiscordInteraction): string | undefined {
  const data = interaction.data;
  if (data === undefined) return undefined;
  if (data.custom_id !== undefined) return data.custom_id;
  const options = data.options;
  if (options === undefined) return undefined;
  return options.find((option) => option.name === "prompt")?.value;
}

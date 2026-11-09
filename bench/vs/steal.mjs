/** The host's steal share between two /cpu readings, in percent. NaN (JSON null) if no ticks passed. */
export const stealBetween = (from, to) => ((to.steal - from.steal) / (to.all - from.all)) * 100;

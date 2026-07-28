package ru.minelauncher.skinsync.network;

import net.neoforged.neoforge.network.handling.IPayloadContext;

import java.util.function.Consumer;

public final class ClientPayloadBridge {
    private static Consumer<SkinDataPayload> dataHandler = payload -> {
    };
    private static Consumer<SkinRemovePayload> removeHandler = payload -> {
    };

    private ClientPayloadBridge() {
    }

    public static void install(
            Consumer<SkinDataPayload> newDataHandler,
            Consumer<SkinRemovePayload> newRemoveHandler
    ) {
        dataHandler = newDataHandler;
        removeHandler = newRemoveHandler;
    }

    public static void handleData(SkinDataPayload payload, IPayloadContext context) {
        dataHandler.accept(payload);
    }

    public static void handleRemove(SkinRemovePayload payload, IPayloadContext context) {
        removeHandler.accept(payload);
    }
}

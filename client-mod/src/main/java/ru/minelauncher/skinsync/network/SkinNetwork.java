package ru.minelauncher.skinsync.network;

import net.neoforged.neoforge.network.event.RegisterPayloadHandlersEvent;
import net.neoforged.neoforge.network.registration.PayloadRegistrar;
import ru.minelauncher.skinsync.server.ServerSkinStore;

public final class SkinNetwork {
    public static final String PROTOCOL_VERSION = "2";

    private SkinNetwork() {
    }

    public static void register(RegisterPayloadHandlersEvent event) {
        PayloadRegistrar registrar = event.registrar(PROTOCOL_VERSION).optional();
        registrar.playToServer(
                SkinHelloPayload.TYPE,
                SkinHelloPayload.STREAM_CODEC,
                ServerSkinStore::handleHello
        );
        registrar.playToServer(
                SkinUploadPayload.TYPE,
                SkinUploadPayload.STREAM_CODEC,
                ServerSkinStore::handleUpload
        );
        registrar.playToClient(
                SkinDataPayload.TYPE,
                SkinDataPayload.STREAM_CODEC,
                ClientPayloadBridge::handleData
        );
        registrar.playToClient(
                SkinRemovePayload.TYPE,
                SkinRemovePayload.STREAM_CODEC,
                ClientPayloadBridge::handleRemove
        );
    }
}

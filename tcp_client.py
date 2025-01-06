import socket
import struct
import cv2
import av
import sys

def main():
    if len(sys.argv) != 3:
        print("Usage: python tcp_client.py <host> <port>")
        sys.exit(1)

    host = sys.argv[1]
    port = int(sys.argv[2])

    decoder = av.CodecContext.create("h264", "r")
    i = 0

    try:
        # Create a socket object
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as client_socket:
            # Connect to the server
            client_socket.connect((host, port))
            print(f"Connected to {host}:{port}")

            # Receive and print data
            while True:
                if i == 10:
                    out = struct.pack("<II", 1920, 1080)
                    client_socket.sendall(out)
                    print("Sent data!")

                b = client_socket.recv(4)
                n_bytes = int.from_bytes(b, byteorder='big', signed=False)
                print(f"Reading {n_bytes}")
                buff = bytearray(n_bytes)

                read = 0
                while read < n_bytes:
                    cr = client_socket.recv_into(memoryview(buff)[read:])
                    if cr == 0:
                        raise EOFError
                    read += cr

                print('\n'.join([bin(x)[2:].zfill(9) for x in list(buff[:8])]))
                packets = decoder.parse(buff)
                print(len(packets))
                for packet in packets:
                    print(packet.is_keyframe)
                    try:
                        for frame in decoder.decode(packet):
                            img = frame.to_ndarray(format="bgr24")
                            print(img.shape)
                            cv2.imwrite(f"out/test_{i:03}.jpg", img)
                            i += 1
                    except av.InvalidDataError:
                        print("Could not decode...")


    except ConnectionError as e:
        print(f"Connection error: {e}")
    except KeyboardInterrupt:
        print("\nConnection closed.")
        sys.exit(0)

if __name__ == "__main__":
    main()

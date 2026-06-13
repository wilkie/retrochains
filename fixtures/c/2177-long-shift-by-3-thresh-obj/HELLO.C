int main(void) {
  long a = 0x12345678L;
  long r = a << 3;
  return (int)(r >> 16);
}

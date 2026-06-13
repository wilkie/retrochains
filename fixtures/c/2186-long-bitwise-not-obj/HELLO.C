int main(void) {
  long a = 0x12345678L;
  long r = ~a;
  return (int)(r >> 16);
}

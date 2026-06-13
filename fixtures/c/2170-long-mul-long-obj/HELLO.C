int main(void) {
  long a = 100000L;
  long b = 200L;
  long r = a * b;
  return (int)(r >> 16);
}

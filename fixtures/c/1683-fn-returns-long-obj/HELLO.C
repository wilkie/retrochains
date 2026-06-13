long big(int x) {
  return (long)x * 1000L;
}
int main(void) {
  long r = big(7);
  return (int)r;
}

int n;
int decr(void) {
  return n--;
}
int main(void) {
  int count = 0;
  n = 3;
  while (decr()) count++;
  return count;
}

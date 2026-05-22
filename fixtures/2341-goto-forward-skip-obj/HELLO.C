int main(void) {
  int x;
  x = 5;
  if (x > 0) goto done;
  x = 99;
done:
  return x;
}

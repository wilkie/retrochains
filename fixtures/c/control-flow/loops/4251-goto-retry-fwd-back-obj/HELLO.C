int main(void) {
  int n;
  int tries;
  n = 0;
  tries = 0;
retry:
  tries = tries + 1;
  n = n + tries;
  if (n >= 6) goto done;
  if (tries < 5) goto retry;
done:
  return n;
}

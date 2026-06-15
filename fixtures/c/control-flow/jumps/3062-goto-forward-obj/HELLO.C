int check(int x) {
  if (x < 0) goto done;
  x = x + 100;
done:
  return x;
}

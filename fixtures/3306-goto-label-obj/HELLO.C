int f(int x) {
  if (x == 0) goto fail;
  return 1;
fail:
  return -1;
}
